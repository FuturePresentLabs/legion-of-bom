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
   3.5 Global parts library (pinout/rating/SPICE-model verification — see also MCP.md)

4. **Validation Layer**
   4.1 ERC (first pass) — what it catches, what it doesn't
   4.2 Project-specific rule extensions (Eurorack power header conventions, panel mount checks)
   4.3 Parts verification gate (filled — see body)

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
   6.9 PanelSpec trait — the format-agnostic seam (filled — see body)

7. **Panel Design**
   7.1 DXF output (filled — see body)
   7.2 SVG output (priority raised — see 7.7)
   7.3 KiCad-native panel workflow
   7.4 Mechanical fit checks against PCB (jack/pot alignment)
   7.5 Manual order tracking — no fab-vendor API available (filled — see body)
   7.6 Visual BOM / component sorting sheet (filled — see body)
   7.7 Drilling jigs + Lightburn SVG (filled — see body)
   7.8 Build-guide rendering (filled — see body)

8. **Manufacturing Outputs**
   8.1 Gerbers via KiCad (v1 path)
   8.2 Direct-to-gerber (future — if/when it's worth it)
   8.3 Output validation before submission

9. **BOM & PCBA Pipeline**
   9.1 BOM generation from IR
   9.2 CPL (component placement list) generation
   9.3 JLCPCB API integration (quoting, order creation, tracking)
   9.4 BOM accuracy/verification — this has to be airtight (filled — see body)
   9.5 Multi-vendor part sourcing rules (JLCPCB basic vs extended, LCSC fallback)
   9.6 Compatible-substitute suggestions (filled — see body)

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

## 1. Vision & [118;1:3uScope

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

### 2.3 Canonical circuit representation
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
No longer empty in practice — the LM13700 bias-generator work (control
voltage → IABC) turned out to be genuinely shared between the slew limiter
and crossfader, the first real evidence of reuse the deferred trigger
condition was waiting for. Extracted into `circuits/lib`, git-native per the
Section 2.6 storage split (circuit content, not structured cross-project
data).

**Each subcircuit entry declares what it needs from the part filling its
role, not just which part was used** — this is what makes substitution
(Section 9.6) possible without a hand-maintained pairwise compatibility
table:

```json
{ "subcircuit": "unity_gain_buffer", "requires": {"min_gbw_mhz": 1, "max_vos_mv": 10} }
{ "subcircuit": "crossfader_signal_path", "requires": {"min_gbw_mhz": 3, "max_vos_mv": 3, "max_noise_nv_hz": 15} }
{ "subcircuit": "ota_bias_generator", "requires": {"part_family": "LM13700-compatible dual OTA"} }
```

A part that's a valid substitute for one role (e.g. TL072 in a buffer) isn't
automatically valid for another (e.g. the crossfader's signal path, which
needs tighter GBW/noise) — the requirement lives with the *role*, not as a
property hung off a specific part pairing.

### 3.4.1 What's shared vs. not — the LM13700 case, concretely
Only the **bias generator** (CV-to-IABC resistor network) is genuinely
reusable between the slew limiter and crossfader — same math, same purpose.
The OTA *application* around it differs and stays circuit-specific: the slew
limiter runs its OTA as a bounded current source (no diode linearization
needed), the crossfader runs its OTA pair as a linear signal path (diode
linearization required, per the derivation in `crossfader-circuit.md`).
Modeling the whole LM13700 circuit as "one subcircuit, reused twice" would
have been wrong — the boundary is narrower than the IC, and getting that
boundary right mattered more than reusing as much code as possible.

### 3.5 Global parts library (pinout/rating/SPICE-model verification)
**Status: in progress — tracked as epic `okm`, 5 P1 tasks open (schema,
distributor sourcing, `fetch_datasheet`, the `verified_by_human` gate,
MPN-based `define_circuit`).** Unblocked by Phase 0's close; the first proven
slice already exists — Phase 0 demonstrated components carrying their own
SPICE models with a resolver seam this library plugs into, ahead of the full
schema being built.

`define_circuit` — whether invoked directly (writing SKiDL by hand), via the
`lob` CLI, or via an MCP-driving agent (see MCP.md) — resolves parts by MPN
against a **global, Dolt-backed parts library**, not model/training memory and
not a per-project file. This is core `legion-of-bom-core` architecture, not an
agent-only construct: the same trust rules apply regardless of entry point.

- **Sourcing order**: distributor-official CAD library (DigiKey's in-house
  KiCad library, JLCPCB/EasyEDA) → broader CAD library (SnapEDA/Ultra
  Librarian, checking verification status) → PDF datasheet extraction as
  fallback, sourced only from a distributor API's datasheet URL field, never a
  general web search. See LIBRARIES.md for the full curated source list.
- **Every extracted pin/rating carries a citation** (source, page/section) —
  see MCP.md Section 1 for the full schema (`parts`, `part_pins`,
  `part_ratings` tables)
- **SPICE models are a distinct artifact from pinout/ratings, sourced
  differently and verified differently.** They typically come from the
  manufacturer directly (e.g. TI's own product page), not a distributor API
  field, and multi-channel packages often need a wrapper subcircuit (the raw
  manufacturer model is single-channel; a dual op-amp needs it instantiated
  twice with matching pin order). Verification here isn't "read the pin
  number correctly" — it's "run a known textbook case and confirm the
  simulated result matches," the same evidentiary bar Phase 0 held its own
  demo circuits to. See MCP.md and the `part_spice_models` table concept for
  schema detail.
- **`verified_by_human` gates real use**: a part can be fetched and extracted
  automatically, but `layout` and `generate_bom` (Section 6, Section 9) refuse
  to run against any part that hasn't been human-confirmed at least once.
  Verification is global and one-time per part — reused across every future
  project, not re-checked per repo
- This exists specifically to prevent a repeat of the LM13700-pinout-from-
  memory mistake made earlier in this project — the fix is structural
  (a gate the pipeline enforces) rather than a process reminder to double-check

**Parts library ≠ BOM — related, not the same epic.** The parts library
answers "is this part definition trustworthy" (pinout, ratings, SPICE model,
keyed by MPN) — no pricing, no quantities, no vendor stock, that's not its
job. BOM generation (Section 9.1) *reads* this library to know which MPNs are
real and verified, then adds something the parts library was never meant to
hold: live pricing/stock from a distributor API (Mouser, to start). Sequencing
consequence: the parts-library epic unblocks BOM, but BOM is a separate
epic layered on top, not a subtask inside this one.

Full detail, tool schemas, and the agent-facing flow live in MCP.md — that
document describes the agent surface of this same core mechanism, not a
separate implementation of it.

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

### 6.9 PanelSpec trait — the format-agnostic seam
Same move as Section 3.3's deferred circuit IR, applied to panels: rather than
designing shared geometry across all four formats now (Eurorack, Guitar
Pedal, Rack Mount, 500-series) — or the reverse mistake of hard-coding
Eurorack-specific concepts (HP, U) directly into layout code — a thin
`PanelSpec` trait is defined now, implemented fully for Eurorack only (what's
actually shipping), with the other formats' implementations deferred until
one of them is actually being built.

**Shape of the trait**: dimensions in mm (each format's native units —
HP/U for Eurorack, enclosure size class for pedals, rack U-height ×
19"-fraction, 500-series' fixed slot — translate into mm through the
implementation, not exposed as universal concepts to the rest of the
pipeline), a mounting-hole list, and a list of anchored cutouts (position,
rotation, footprint reference) that Section 7's DXF export and Section 6's
layout loop both read through the trait rather than assuming Eurorack.

**Why this isn't "design all four now" or "hard-code Eurorack now"**: the
four formats aren't similar enough to unify speculatively. 500-series in
particular is mechanically the odd one out — audio I/O typically runs through
a card-edge connector into the shared lunchbox bus, not individual
front-panel jacks the way Eurorack/pedal/rack all work — forcing it into the
same anchored-jack-pot cutout model used for the other three would be wrong,
not just incomplete, so its `PanelSpec` implementation waits until it's
actually being built rather than being guessed at now.

---

## 4. Validation Layer

*(4.1 ERC and 4.2 project-specific rule extensions remain TOC-only — filling
in 4.3 out of order because it's a cross-cutting concern that also touches
Sections 3 and 9)*

### 4.3 Parts verification gate
Before `layout` (Section 6) or `generate_bom` (Section 9) run, validation
checks that every part referenced in the circuit has `verified_by_human =
TRUE` in the global parts library (Section 3.5) and that the circuit's
open-questions checklist has no unresolved items. This is enforced inside
`legion-of-bom-core` itself — identical behavior whether the pipeline is
driven by the `lob` CLI directly or by an agent through MCP (see MCP.md
Section 1.4 and 2.2). Failing either check blocks the run and surfaces the
specific unresolved items rather than proceeding on unverified data — the
structural fix for the LM13700-pinout-from-memory mistake made earlier in
this project.

---

## 7. Panel Design

*(7.2–7.4 remain TOC-only — filling in 7.1 and 7.5 now since they're the
concrete near-term ask: get a real DXF out, and know whether it's been
ordered, given no fab vendor offers API automation yet)*

### 7.1 DXF output
Panel outline and cutouts (jack holes, pot holes, mounting holes, any
silkscreen-equivalent engraving) are exported to DXF from the same
`kicad-ipc-rs` connection used for the board itself (Section 6.6) — the panel
is its own KiCad PCB-editor-adjacent artifact (either a dedicated mechanical
layer/outline within the project, or a separate panel-only board file
depending on how the `PanelProfile`, Section 6.1, is implemented), with
geometry driven by the per-module physical layout (jack/pot/LED positions,
HP width) that DESIGN.md flagged as a real, currently-undesigned input.
`kicad-cli`'s existing plot/export tooling handles the actual DXF write —
same already-headless CLI path used for Gerber export in 6.6, not gated by
the "KiCad GUI must be running" requirement that applies to interactive
IPC operations.

DXF is the priority format because it's what both SendCutSend and OSH Cut
actually want as upload input (confirmed via their own tutorials/workflows) —
SVG (7.2) is lower priority until there's a concrete reason to need it.

### 7.5 Manual order tracking — no fab-vendor API available
Neither SendCutSend nor OSH Cut expose a public order-submission API (confirmed
— both are upload-a-file-to-a-web-app workflows only), unlike JLCPCB/Mouser/
DigiKey. So panel ordering itself stays a manual step: `legion-of-bom`
generates the correct DXF, a human uploads it and places the order on the
vendor's site. What *is* automatable is tracking status of that manual step,
so it doesn't get lost the way an untracked step easily does.

**Storage: a `panel_orders` table in the same Dolt-backed store used for
inventory (Section 2.6, Section 11)** — this is structured, queryable,
cross-project data, the same category as parts verification and inventory,
not project-specific circuit content that belongs in git.

```sql
CREATE TABLE panel_orders (
  id INT PRIMARY KEY,
  module VARCHAR(64) NOT NULL,       -- e.g. "crossfader-v1"
  dxf_path TEXT NOT NULL,            -- path/commit ref to the generated DXF
  vendor VARCHAR(32),                -- "sendcutsend" | "oshcut" | other
  status VARCHAR(16) DEFAULT 'not_ordered',  -- not_ordered | ordered | shipped | received
  ordered_at DATETIME,
  tracking_ref TEXT,                 -- vendor order/tracking number, entered manually
  notes TEXT
);
```

`lob panel status <module>` reads current state; `lob panel mark-ordered
<module> --vendor oshcut --tracking <ref>` updates it — simple manual CLI
commands standing in for what an API webhook would otherwise do automatically.
**Explicitly deferred, not designed away**: if either vendor adds a real API
later, or if this becomes a big enough part of operations to justify it, the
manual `mark-ordered` step gets replaced by an actual API call updating the
same table — the schema doesn't need to change, just how `status` gets set.

### 7.6 Visual BOM / component sorting sheet
Same underlying data as Section 9.1's BOM — grouped and rendered for a human
sorting physical parts by hand, not for procurement. No new data dependency:
this is a rendering template layered on the existing BOM output, cheapest
item in this set of additions. General enough to apply to any format, not
Guitar-Pedal-specific.

### 7.7 Drilling jigs + Lightburn SVG
Two related outputs, both consuming data `PanelSpec` (Section 6.9) already
produces:

- **3D-printable drilling jigs** — STL generated from the anchored-cutout
  hole positions already computed for DXF export (7.1), targeted at a small,
  fixed library of standard enclosure sizes (1590B, 1590BB, 125B, etc.) —
  exactly the kind of format-specific standardization Guitar Pedal has and
  Eurorack (bespoke per-module panels) mostly doesn't, which is why this
  lands here rather than as a general Section 7 feature.
- **Laser SVG for Lightburn** — this is Section 7.2 (SVG output), previously
  deprioritized for lack of a concrete use case. Pedal enclosure engraving
  and laser-cut drilling templates are that use case — priority raised for
  the Guitar Pedal line specifically, not a general re-prioritization of 7.2.

Both are alternate exports of geometry the pipeline already computes for DXF
— real work, but not a new data problem.

### 7.8 Build-guide rendering
The one genuinely new subsystem in this set, not a reformat of existing data.
Needs two things that don't exist elsewhere in the pipeline yet:

- **Rendered board views** — a populated-board 3D render (KiCad's rendering
  capability, same general approach `protorack-kicad`/`pcb2blender` use per
  LIBRARIES.md) with a highlight box overlaid at the current step's footprint
  position. Deliberately not real photography — avoids needing physical units
  reshot per board revision, which real photos would require.
- **Build sequencing** — a step order (typically low-profile-first: resistors
  → diodes → sockets → caps → tall/tall-bodied parts last) that has to be
  derived or specified per circuit; nothing in the pipeline currently produces
  this. Real new scope, sized as its own small epic rather than a BOM-export
  add-on.

Scope note for all of 7.6–7.8: layered on top of the Guitar Pedal `PanelSpec`
implementation, not before it exists — same gating already applied to
500-series and the rest of the layout epic.

---

## 9. BOM & PCBA Pipeline

*(9.1–9.3 and 9.5 remain TOC-only — filling in 9.4 out of order for the same
reason as 4.3 above. Note: 9.1 depends on the parts library (Section 3.5,
epic `okm`) but is its own separate epic — BOM adds live distributor pricing
on top of the parts library's verified-MPN data, it doesn't live inside the
parts-library epic. Mouser-priced BOM CSV is the current priority for 9.1.)*

### 9.4 BOM accuracy/verification — this has to be airtight
BOM generation reads part records (pricing, stock, footprint) from the same
global parts library used by circuit definition and layout (Section 3.5), not
a separate lookup. Practical consequence: a part's `verified_by_human` status
is not just a layout-time gate — `generate_bom` checks it too, for the same
reason. An unverified pinout is a layout risk; an unverified part record feeding
a real JLCPCB/Mouser/DigiKey order is a money risk on top of that, since a
misidentified part (wrong package, wrong footprint, wrong voltage rating) can
turn into a bad PCBA order rather than just a bad board file. Same gate,
enforced at the same core layer, for both reasons at once.

### 9.6 Compatible-substitute suggestions
**Advisory only — never automatic.** A pin-compatible part is not
automatically an acceptable substitute for a specific circuit; it's a
candidate that still has to clear the requirements of the *role* it would
fill (Section 3.4's subcircuit `requires` blocks). Two-part mechanism:

- **`pin_compatible_groups`** — a small, hand-maintained table of physically
  drop-in part families (op-amp pinout-compatible groups, etc.), each with
  its own `verified_by_human` flag. This is the only genuinely stable fact
  worth caching, since footprint/pinout compatibility doesn't change.
  ```sql
  CREATE TABLE pin_compatible_groups (
    group_id INT,
    mpn VARCHAR(64),
    package VARCHAR(32),
    verified_by_human BOOLEAN DEFAULT FALSE
  );
  ```
- **Live requirement check, not a stored relation** — at `generate_bom` time,
  if a primary MPN has low/zero stock, pull every part sharing its
  `pin_compatible_groups` entry and check each against the *subcircuit's*
  declared requirements (using the already-stored `part_ratings` from
  Section 3.5) — not a hardcoded "part X substitutes for part Y" fact, which
  would silently ignore that the same substitution can be fine in one role
  (a buffer) and wrong in another (a noise-sensitive signal path). Passing
  candidates are surfaced to the human for approval at BOM time; nothing
  swaps automatically.

**Explicitly out of scope for now**: automatic board re-layout in response to
a substitute with a different footprint ("intelligent respin"). Depends on
the iterative layout loop (Section 6.5) being mature enough to safely
re-run place/route/DRC unattended, which it isn't yet — revisit once layout
itself is solid, not before.
