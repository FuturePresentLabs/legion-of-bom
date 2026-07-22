# Puget Audio Board Pipeline — DESIGN.md

Status: DRAFT — sections 1-3 filled in, 4-15 pending.

## Table of Contents

1. **Vision & Scope**
   1.1 Problem statement
   1.2 What "done" looks like
   1.3 Non-goals (v1)

2. **Architecture Overview**
   2.1 Pipeline stages (map to the 8-step loop)
   2.2 Orchestration model (CLI tool? library? workflow engine?)
   2.3 Canonical circuit representation (the IR that DSLs compile to / tools read from)
   2.4 Repo & project structure
   2.5 Multi-tenancy & auth
   2.6 Storage split: Dolt vs. git-native

3. **Circuit Definition Layer**
   3.1 SKiDL (v1, Python)
   3.2 Path to a native Rust DSL (v2+)
   3.3 DSL-agnostic circuit IR — what it must capture to support multiple frontends
   3.4 Reusable component/subcircuit library (op-amp stages, OTA stages, RIAA networks, etc.)

4. **Validation Layer**
   4.1 ERC (first pass) — what it catches, what it doesn't
   4.2 Project-specific rule extensions (Eurorack power header conventions, panel mount checks)

5. **Simulation Layer**
   5.1 ngspice integration
   5.2 PedalKernel integration — reusing the existing accuracy/harness infrastructure
   5.3 What gets simulated pre-layout vs. what waits

6. **Layout & DFM**
   6.1 Format profiles (Eurorack, Guitar Pedal, Rack Mount, Desktop) — anchored connectors
   6.2 2-layer convention (ground pour default)
   6.3 Mode: Analog vs. Digital — cost-function weighting
   6.4 Critical-net tagging in SKiDL
   6.5 Iterative layout loop (place → route → check → repair)
   6.6 KiCad pcbnew integration (from IR/netlist to board file)
   6.7 DFM checks (JLCDFM + mechanical/3D collision checks)
   6.8 Manual layout escape hatch — where the human loop stays in

7. **Panel Design**
   7.1 DXF output
   7.2 SVG output
   7.3 KiCad-native panel workflow
   7.4 Mechanical fit checks against PCB (jack/pot alignment)

8. **Manufacturing Outputs**
   8.1 Gerbers via KiCad (v1 path)
   8.2 Direct-to-gerber (future — if/when it's worth it)
   8.3 Output validation before submission

9. **BOM & PCBA Pipeline**
   9.1 BOM generation from IR
   9.2 CPL (component placement list) generation
   9.3 JLCPCB API integration (quoting, order creation, tracking)
   9.4 BOM accuracy/verification — this has to be airtight
   9.5 Multi-vendor part sourcing rules (JLCPCB basic vs extended, LCSC fallback)

10. **Thru-Hole BOM & Ordering**
    10.1 Distributor sourcing (LCSC/Mouser/DigiKey)
    10.2 Kitting for in-house assembly at FPL

11. **Part Stock & Inventory Management**
    11.1 Data model (parts, boards, revisions, quantities on hand)
    11.2 Shared-component tracking across SKUs (2OPFM, PHRSR/SCANNER/EG/TVCA, future Winterbloom line)
    11.3 Source of truth — where inventory state actually lives

12. **Reorder Automation**
    12.1 Trigger logic (thresholds, lead-time awareness)
    12.2 n8n integration
    12.3 Human-in-the-loop vs. fully automatic ordering

13. **QC, Test & Revision Control**
    13.1 Bench test / functional test procedure before ship
    13.2 Board revision scheme (silkscreen rev ↔ git tag ↔ IR version)
    13.3 Traceability (which rev shipped to which retailer/order)

14. **Roadmap & Milestones**
    14.1 Phase 0 — demo circuits (RC low-pass filter, then non-inverting op-amp gain
         stage) as end-to-end proof of concept, local-only. Goal: exercise every
         pipeline stage on textbook circuits with known-good answers before risking
         a real product design on an unproven pipeline
    14.2 Phase 1 — slew limiter (first real Puget Audio board), still local/localhost
    14.3 Phase 2 — MVP hardening covering must-have outputs (KiCad/PedalKernel/SPICE)
    14.4 Phase 3 — panel outputs (DXF/SVG/KiCad)
    14.5 Phase 4 — BOM/PCBA hardening + inventory (Dolt) + reorder automation
    14.6 Phase 5 — SaaS-ification: multi-tenancy, auth, hosted remote-repo connections
    14.7 Phase 6 — direct-to-gerber, additional DSLs (stretch)

15. **Open Questions**
    (running list — filled in as we go)

---

## 1. Vision & Scope

### 1.1 Problem statement
Small electronics hardware businesses (starting with Puget Audio) currently manage
circuit design, simulation, layout, BOM/PCBA ordering, and inventory across
disconnected tools — KiCad GUI, spreadsheets, manual retailer ordering. legion-of-bom
unifies this into one pipeline: circuit-as-code in, manufacturing-ready outputs and
live inventory/reorder state out.

### 1.2 What "done" looks like
Phase 0: an RC low-pass filter and a non-inverting op-amp gain stage, each defined in
SKiDL, flow through the full pipeline — validation, ngspice simulation checked against
textbook predicted values, KiCad layout scaffolding, BOM/CPL — entirely on one local
machine, no network-facing component. That proves the loop before any real Puget
Audio product design goes through it. Full "done" (later phases): a user defines a
circuit in SKiDL (or a future DSL) inside a git repo; legion-of-bom runs the same
pipeline, can quote and place a JLCPCB PCBA order via their API, tracks inventory in
Dolt, and triggers reorders — through a web dashboard, usable by more than one
tenant/business.

### 1.3 Non-goals (v1)
- Full autorouting/autoplacement — manual layout stays human-driven initially
- Replacing KiCad's schematic/layout editors outright — legion-of-bom orchestrates
  around them, doesn't replace them
- Multi-tenancy, hosted auth, remote repo connections, billing — v1 is local-only,
  single-user, no network-facing auth surface at all. SaaS-shaping is an
  architectural intent for later, not a v1 requirement

---

## 2. Architecture Overview

### 2.1 Pipeline stages
Maps to the 8-step loop (Sections 4–13 below), plus panel design and QC/revision
control as first-class stages rather than afterthoughts.

### 2.2 Orchestration model
A Rust core library (`legion-of-bom-core`) exposes each pipeline stage as a composable
function/trait. A CLI (`lob`) wraps the library for local, scriptable use. A web
backend (axum — same stack as Understory) wraps the *same* library for the hosted
dashboard, so nothing is web-only — anything the dashboard can do, the CLI can do
headless.

### 2.3 [118;1:3uCanonical circuit representation
Deferred per decision: SKiDL-native for now, IR extracted once a second DSL is real.
Flag for later sections: pipeline stages downstream of circuit definition
(validation, sim, layout) should still be written against a thin internal interface
now, even before extraction, so the eventual IR refactor doesn't ripple through
everything that consumes circuit data.

### 2.4 Repo & project structure
- **FuturePresentLabs/legion-of-bom** — the tool itself (core lib + CLI + web
  backend), AGPLv3
- **Board/circuit repos** — one per project (e.g. `puget-audio-2opfm`,
  `puget-audio-slew-limiter`), each can hold multiple related circuits, plain git.
  In v1, these are local clones on the machine running `lob` — the CLI/backend
  shells out to `git` directly, no remote-hosting integration yet. The
  Forestry.io-for-KiCad model (connecting to *someone else's* remote repo) is a v2+
  concern once SaaS-ification happens
- **Inventory data** — lives in Dolt, not in the circuit git repos (see 2.6)

### 2.5 Multi-tenancy & auth (deferred to a later phase)
v1 is local-first and single-user: the CLI runs git commands directly against
locally-cloned repos, and the web dashboard runs on localhost with no auth layer at
all. Multi-tenancy, hosted repo connections, and auth (WebAuthn passkeys vs GitHub
OAuth vs something else) only get designed once the core pipeline loop — schematic
through inventory — actually works end-to-end for one user. Not designing this now
on purpose; revisit in Section 14 roadmap.

### 2.6 Storage split: Dolt vs. git-native
Circuit definitions (SKiDL files, KiCad projects) stay in plain git — they're
file-based already and git's native diff/PR tooling works well on them. Inventory
data (parts, stock levels, shared components across SKUs, reorder thresholds) lives
in Dolt instead — a SQL database that's version-controlled the same way git is
(branches, commits, diffs, merges, all addressable via refs). This gives inventory a
real query/aggregation layer (which plain files can't do well) while keeping the
same versioning model as the rest of the system.

---

## 3. Circuit Definition Layer

### 3.1 SKiDL (v1, Python)
SKiDL is the circuit-definition frontend for Phase 0 and Phase 1. Each circuit is a
Python script using SKiDL's `Part`/`Net`/`Pin` API; running the script produces a
netlist (`generate_netlist()`) that `legion-of-bom-core` shells out to KiCad's
`pcbnew` Python API to turn into a board file. SKiDL also runs `ERC()` for first-pass
validation before anything reaches netlist generation (Section 4).

### 3.2 Path to a native Rust DSL (v2+)
Explicitly deferred. Not designed now, not blocking Phase 0/1. When it happens, it
targets whatever canonical IR gets extracted in 3.3 — same target SKiDL will be
retrofitted onto, not a parallel one-off format.

### 3.3 DSL-agnostic circuit IR
Deferred per the Section 2.3 decision: SKiDL-native now, IR extracted once a second
DSL is real. Practical consequence for Phase 0 build: `legion-of-bom-core`'s
validation/simulation/layout stages should consume circuit data through a small
internal trait (e.g. something like a `CircuitSource` trait returning parts/nets)
rather than calling SKiDL-specific types directly everywhere — even though the only
implementation right now is "read a SKiDL-generated netlist." This keeps the eventual
IR extraction to one new implementation of that trait instead of a rewrite.

### 3.4 Reusable component/subcircuit library
Starts empty. Phase 0's two demo circuits are written as standalone SKiDL scripts,
not yet abstracted into a library — deliberately, so real reuse patterns emerge from
having 2+ circuits before anything gets extracted into a shared `circuits/lib`
module. Revisit after Phase 0 and the slew limiter (Phase 1) are both done — by then
there'll be enough real examples (RC filter, op-amp gain stage, OTA slew stage) to
know what's actually common.

---

## 6. Layout & DFM

*(Sections 4–5 remain TOC-only placeholders — filled out of order because this was
the harder architectural problem to work through)*

### 6.1 Format profiles (Eurorack, Guitar Pedal, Rack Mount, Desktop) — anchored connectors
v1 scope is deliberately limited to four form factors rather than general PCB
layout. Each format gets a `PanelProfile`: mounting hole pattern, panel
dimension convention (Eurorack HP, pedal enclosure size classes, rack U-height,
desktop footprint), and a library of standard connectors with fixed footprints
and 3D models. Critically, connectors defined by a format's `PanelProfile`
(jacks, pots, switches) are **placement-anchored** — their position is
determined by the panel spec, not by the layout optimizer. This shrinks the
free-placement problem to just the support components (passives, ICs) instead of
the whole board, which is what makes an iterative loop tractable instead of a
general NP-hard placement search.

### 6.2 2-layer convention (ground pour default)
Hard v1 constraint: 2 layers only, matching real fabrication experience and no
near-term need for more. Default convention: bottom layer is a solid ground
pour, top layer carries signal and power traces, with via stitching down to
ground where needed. This substantially replaces manual star-ground topology
work for most nets — a solid pour gives a low-impedance return path largely "for
free," rather than requiring every ground connection to be individually routed
to converge at one point. Reduces one of the most error-prone parts of analog
layout (see the phono-preamp grounding discussion) to a default that mostly just
works, with critical-net tagging (6.4) as the escape hatch for the nets where it
doesn't.

### 6.3 Mode: Analog vs. Digital — cost-function weighting
Project-level mode selection (not auto-detected) picks which violation-scoring
weights the iterative loop (6.5) uses:
- **Analog mode:** weights ground-pour integrity, critical-net proximity
  (feedback networks, decoupling caps within a short distance of IC power pins),
  and channel symmetry (stereo matching)
- **Digital mode:** weights trace length, via count, and (later) controlled
  impedance / differential pair matching
- **Mixed:** both weight sets active, with per-net critical tagging (6.4) doing
  the heavy lifting to tell the loop which rules apply where

### 6.4 Critical-net tagging in SKiDL
Manual, not auto-detected (confirmed decision). The circuit author explicitly
tags nets that need analog-careful treatment at definition time — e.g. a
feedback network net, a high-impedance input stage net, a matched-pair net for
stereo channels. Auto-detection from topology was considered and rejected:
whether a net is "critical" often depends on domain judgment (audio-rate,
high-impedance, sensitive to parasitic coupling) that isn't cleanly inferable
from graph structure alone. Practical shape: something like a `critical()`
wrapper or tag argument on the net when defined in SKiDL, carried through into
the netlist/IR so the layout loop can read it without re-deriving it.

### 6.5 Iterative layout loop (place → route → check → repair)
Not general autorouting — guided iterative repair over the *free* (non-anchored)
components and nets only:
1. Auto-place free components, seeded near their anchored neighbors (e.g. a
   decoupling cap seeded close to the IC it decouples)
2. Auto-route non-critical nets against the current mode's cost weights
3. Check violations: electrical DRC, mechanical/3D collision checks (6.7),
   critical-net rules (e.g. "is this tagged critical net physically short and
   direct, or did the router send it around the board")
4. If violations exist, generate a targeted repair (not a full re-place) and
   recheck
5. Exit condition is **not** "zero violations" — critical nets that the loop
   can't confidently resolve get surfaced explicitly for manual routing, not
   silently left broken or silently "solved" by a heuristic that shouldn't be
   trusted on that net

Each attempt is a natural git commit (circuit repos are already git-native),
which gives the loop's history for free — diffable, revertable, no separate
state-tracking needed.

### 6.6 KiCad pcbnew integration (from IR/netlist to board file)
**Decision: `kicad-ipc-rs`, talking to KiCad's IPC API — not the SWIG-based
Python `pcbnew` bindings.** SWIG is technically usable on the current stable
release (KiCad 10) but is deprecated as of KiCad 9, in maintenance mode only,
and scheduled for full removal in KiCad 11 — building `legion-of-bom-core`
against it means a forced rewrite the moment KiCad is upgraded. The IPC API is
KiCad's actively-developed, forward-looking programmatic interface and is
where `kicad-python` (the official Python equivalent) already lives.

**v1 connection mode: attach to a running KiCad GUI instance**, not a headless
server. The IPC API in KiCad 9/10 only supports talking to an already-running
KiCad session — there is no headless option yet on the current stable release.
Rather than standing up Docker/xvfb to fake a headless environment (real
overhead, especially unpleasant on macOS), v1 simply requires KiCad open
locally and connects `kicad-ipc-rs` to it — a reasonable requirement given
`legion-of-bom` is already local-first and single-user (Section 2.5), and this
matches the actual working setup rather than solving a CI/server problem that
doesn't exist yet.

**Known upgrade path, not a maybe**: `kicad-cli api-server` (headless IPC
server, no GUI) ships as of KiCad 11, and `kicad-python` already has a
`headless=True` connection mode built specifically around it — same IPC
protocol, just a different socket to connect to. When `legion-of-bom` needs
true headless (CI, the eventual SaaS phase in 14.6), the change is which
socket `kicad-ipc-rs` connects to, not a rewrite of how it talks to KiCad.
Document this here so it isn't re-litigated later: **v1 requires KiCad running
locally, on purpose, with a known and already-supported path off of that
requirement when it's actually needed.**

**Mechanics**: netlist (from SKiDL, Section 3) → `kicad-ipc-rs` client calls
into the running KiCad instance to create/update the board, place footprints
(anchored per Section 6.1, free-placed per the iterative loop in 6.5), and
drive routing attempts → violations read back via the same IPC connection
(Section 6.5 step 3) → once clean, Gerbers exported via `kicad-cli pcb export
gerbers` (a separate, already-headless CLI command, not gated by the
GUI-instance requirement above).

### 6.7 DFM checks (JLCDFM + mechanical/3D collision checks)
Two kinds of check: standard electrical/manufacturing DFM (JLCDFM or
equivalent — trace/space rules, hole sizes, etc.) and mechanical collision
checks enabled by having 3D models of every component — catching a jack body
colliding with a neighboring part, or a panel cutout misaligned with its PCB
footprint, before it becomes a physical mistake instead of just an electrical
one.

### 6.8 Manual layout escape hatch — where the human loop stays in
Any net the iterative loop can't resolve with confidence (per its exit
condition in 6.5) gets flagged for manual routing rather than forced through a
heuristic. Given the RIAA/phono-style circuits discussed earlier are exactly
the case where layout mistakes are audible and hard to diagnose after the fact,
this escape hatch is treated as a feature, not a shortfall — the loop's job is
to auto-resolve what's mechanically resolvable and clearly surface what needs
judgment.
