# legion-of-bom — MCP Interface (MCP.md)

Status: DRAFT. Companion to DESIGN.md — this covers the agent-facing MCP surface,
not the core pipeline internals.

Design principle, stated plainly: an agent should be able to go from a prompt
("design me a modular crossfader in 1U, quality parts") all the way through
research, circuit definition, simulation, layout, and BOM — fully autonomously.
It should **not** be able to place a real order without a human. And nothing in
between should be allowed to *look* verified without actually being verified —
the LM13700-pinout-from-memory mistake earlier in this project is the failure
mode this whole doc is trying to structurally prevent, not just discourage.

---

## 1. Why datasheet reliability is the load-bearing problem

Everything else in this doc is plumbing. This part is the actual risk. An LLM
stating a pinout from training memory is indistinguishable, in tone, from an
LLM stating a pinout it actually read off a real datasheet — that's exactly
what went wrong when the slew limiter's LM13700 pins were drafted "from
memory" earlier in this project. The fix has to be structural, not a reminder
to "double check."

**Rule: circuit definition tools may not accept pin/parametric data from model
memory. Only from a fetched, cited source.**

### 1.1 Sourcing pinout/symbol data: structured libraries first, PDF as fallback
Two kinds of source exist, and they shouldn't be treated as equivalent:

- **Structured CAD library data** — DigiKey maintains an official, in-house
  KiCad library built by their own applications engineering team; JLCPCB's
  EasyEDA/LCSC library is reachable through the same Parts API already in use
  and has existing conversion tooling into KiCad format; SnapEDA/Ultra
  Librarian/SamacSys host a much larger but more variable-trust set (mixed
  professional and community-contributed content, with a verification badge
  worth checking). All of these already encode pin-number-to-pin-name mapping
  as structured data — someone had to get it right to use it in a schematic
  tool at all — which is a fundamentally better extraction source than parsing
  it back out of PDF prose.
- **PDF datasheets** — still the source of truth for absolute max ratings and
  anything not captured in a symbol file, and useful as a cross-check against
  a CAD library entry, but not the primary pinout-extraction path when a
  structured library entry already exists.

**Trust order for `fetch_datasheet` / pinout extraction:** distributor-official
library (DigiKey KiCad library, JLCPCB/EasyEDA) → broader CAD library
(SnapEDA/Ultra Librarian, checking verification status) → PDF datasheet
extraction as the fallback when no library entry exists yet, and always as a
cross-check source for ratings regardless of which library was used for the
pinout. The datasheet URL itself, when PDF parsing is needed, still comes only
from a distributor API record (JLCPCB/Mouser/DigiKey), never a general web
search — that part of the rule doesn't change.

### 1.2 Fetch and extract, don't recall
`fetch_datasheet` (Section 2) retrieves structured pin/footprint data from a
CAD library per the trust order above, or extracts text/tables from a PDF
datasheet when no library entry exists — pin assignment tables, absolute max
ratings, package info. Either way, this is a real fetch-and-parse operation,
not a cached "the model already knows this part" shortcut. Extracted data is
written to a **global, Dolt-backed parts library** (not a per-project git
sidecar file) — this is **core `legion-of-bom-core` architecture, not an
MCP-only construct** (see Section 1.4 below): once a part is verified, it's
verified for every future project and every entry point (CLI, MCP, future web
UI alike), not re-verified per repo or per interface. This mirrors the
Dolt-vs-git storage split already decided in DESIGN.md Section 2.6: circuit
definitions are project-specific and belong in git; verified part data is
cross-project structured data that needs to be queried and reused, which is
exactly what Dolt is for. Versioned via Dolt's refs the same way everything
else is — each verification event is a commit, diffable and revertable like
any other change in the system.

**Proposed schema (Dolt tables):**
```sql
CREATE TABLE parts (
  id INT PRIMARY KEY,
  mpn VARCHAR(64) NOT NULL,          -- manufacturer part number, canonical key
  manufacturer VARCHAR(128),
  datasheet_url TEXT NOT NULL,       -- from distributor API, never free-form search
  fetched_at DATETIME,
  verified_by_human BOOLEAN DEFAULT FALSE,
  verified_at DATETIME,
  verified_by VARCHAR(64)
);

CREATE TABLE part_pins (
  part_id INT REFERENCES parts(id),
  pin_number VARCHAR(8),
  pin_name VARCHAR(64),
  cited_page INT                     -- page in the source datasheet this came from
);

CREATE TABLE part_ratings (
  part_id INT REFERENCES parts(id),
  rating_name VARCHAR(64),           -- e.g. "Vcc_max", "Iabc_max"
  value TEXT,
  unit VARCHAR(16),
  cited_page INT
);
```
A circuit's SKiDL definition references parts by `mpn`, and `define_circuit`
looks up verified data from this global table rather than any per-project
file. First use of a new part still triggers the one-time human verification
step (Section 3); every subsequent project reusing that MPN reads
already-verified data for free.

### 1.3 Every extracted fact carries a[118;1:3u citation, not just a value
Every row in `part_pins` and `part_ratings` carries a `cited_page` back to the
source datasheet — never a bare value with no traceable origin. `parts.
verified_by_human` starts `FALSE` on every new part row. This is the field
the "resolved open questions" gate (Section 3) actually checks — not "did we
fetch a datasheet" but "did a human confirm the extraction was read
correctly."

### 1.4 This is core `legion-of-bom-core` architecture, not an MCP-only construct
This is worth stating plainly because it changes where the enforcement actually
lives: the parts library, the `verified_by_human` gate, and the checks that
block `layout`/`generate_bom` are **pipeline-core behavior**, not something
bolted onto the MCP tool wrapper. An MCP tool call and a direct `lob layout`
CLI invocation hit the exact same check inside `legion-of-bom-core` — the MCP
tool doesn't independently re-implement the gate, it just inherits it by
calling into the same library (per DESIGN.md Section 2.2's "CLI and web share
one core" principle). Practical consequence: DESIGN.md's Validation Layer
(Section 4) and BOM & PCBA Pipeline (Section 9) need this gate specified
there too, not just here — MCP.md describes the *agent-facing* shape of a
check that has to exist regardless of who or what is driving the pipeline.

### 1.5 Cross-check on anything safety/money-relevant
For pin assignments and absolute max ratings specifically (the two categories
where an error causes physical damage or a bad board respin), the extraction
tool should attempt to fetch from **two independent sources** where possible
(e.g. a distributor-official CAD library entry and the manufacturer's own PDF)
and flag a mismatch rather than silently picking one. Parametric data that's
just "nice to know" (typical values, graphs) doesn't need this — the gate is
proportional to what breaks if it's wrong.

### 1.6 What this does NOT solve
This makes pinout/rating *extraction* reliable. It does not make *topology
selection* reliable — an agent can correctly extract a real LM13700 pinout and
still put it in a bad circuit. That's what `research_topology`'s citation
requirement (Section 2) and the human-in-the-loop sim review are for —
datasheet reliability and circuit correctness are separate problems.

---

## 2. MCP Tools

All tools run against the local `legion-of-bom-core` (Section 2.2 of
DESIGN.md) — this is the local-first MCP server, not a hosted SaaS surface.

| Tool | Purpose | Gated? |
|---|---|---|
| `create_project` | Scaffold a new board repo from a `PanelProfile` (format: Eurorack/Pedal/Rack/Desktop) | No |
| `research_topology` | Search for real reference circuits for a given function (e.g. "crossfader"); returns candidate topologies + citations | No |
| `search_part` | Resolve a part number or functional need to distributor records (JLCPCB/Mouser/DigiKey), including datasheet URL | No |
| `fetch_datasheet` | Fetch structured pin/rating data (CAD library or PDF fallback, per Section 1.1) into the global parts library | No |
| `define_circuit` | Write/update the SKiDL circuit definition, referencing verified part data (not model memory) | No |
| `validate` | Run ERC | No |
| `simulate` | Run ngspice / PedalKernel simulation | No |
| `layout` | Run the iterative layout loop (DESIGN.md Section 6) | **Yes — enforced by `legion-of-bom-core` (Section 1.4), same for CLI and MCP: requires open-questions checklist clear + all used parts `verified_by_human = TRUE`** |
| `generate_bom` | Generate BOM/CPL with live pricing/stock from JLCPCB, Mouser, DigiKey APIs | **Yes — same core-enforced gate as `layout`** |
| `submit_for_approval` | Package design + BOM + cost + sourcing status into a human-reviewable summary | No (this *is* the human touchpoint) |
| `place_order` | Actually submit a PCBA/parts order | **Human-only. Not agent-callable. No MCP tool exists for this — it's a manual action outside the MCP surface entirely.** |

### 2.1 On `place_order` specifically
This isn't a permissions flag that could theoretically be toggled — there is
no `place_order` MCP tool. If we ever build one, that's a deliberate,
separate decision, not a default the agent surface grows into by accident.

### 2.2 The verification gate, concretely
Before `layout` or `generate_bom` run — whether invoked via MCP or the `lob`
CLI directly, since this lives in `legion-of-bom-core` per Section 1.4, not
in the MCP wrapper — the check confirms:
- Every part referenced in the circuit has a `parts` row with
  `verified_by_human = TRUE` in the global library
- The circuit's open-questions checklist (per the pattern in
  `slew-limiter-circuit.md`) has no unresolved items
If either check fails, the tool returns the specific blocking items rather
than running anyway — the agent's job at that point is to either resolve them
(re-fetch, ask you to verify) or surface them to you, not proceed.

---

## 3. Human verification step (how `verified_by_human` actually gets set)

A lightweight review, not a full datasheet read-through: the agent presents
the extracted pinout/ratings table plus a link/citation to the source page,
you confirm or correct it once per part (not once per circuit — a part
verified in one project stays verified for reuse elsewhere, per the global
Dolt-backed parts library in Section 1.2). This is the same shape as the
slew limiter's open-questions checklist, just tool-enforced instead of
doc-enforced.

---

## 4. Example flow: "design me a crossfader in 1U, quality parts"

1. `create_project` — new repo, Eurorack `PanelProfile`, mode: Analog
2. `research_topology("crossfader")` — returns 2-3 real reference topologies
   (e.g. dual-VCA constant-power crossfade designs) with citations
3. You pick one (or the agent proposes its recommendation, you confirm)
4. `search_part` + `fetch_datasheet` for each part the chosen topology needs
5. **You verify pinouts/ratings once per new part** (existing parts like
   TL072 may already be verified from earlier circuits)
6. `define_circuit` — SKiDL written against verified data from the global
   parts library
7. `validate`, `simulate` — iterate as needed
8. `layout` — gate passes (all parts verified, checklist clear), iterative
   loop runs
9. `generate_bom` — real pricing/stock pulled
10. `submit_for_approval` — you get a real summary: schematic, sim results,
    layout, BOM with actual cost
11. You place the order yourself. Not a tool call.

Steps 1–10 are genuinely "no sweat" for you — research, design, sim, layout,
and costing all happen without you touching a keyboard except the one-time
per-part verification in step 5. Step 11 stays yours on purpose.

---

## Open questions

- [ ] Where does `research_topology` search — general web search with citation
      requirements, or a curated set of trusted sources (manufacturer app
      notes, established module-design references)? Unbounded web search risks
      pulling in bad reference designs with the same false-confidence problem
      as datasheet recall.
- [ ] What happens when `research_topology` can't find a good reference for a
      genuinely novel circuit — does the agent refuse, or flag lower
      confidence and proceed with more scrutiny requested at the verification
      step?
- [x] **Open-core licensing shape — resolved.** Public repo
      (`FuturePresentLabs/legion-of-bom`) stays AGPLv3 with a CLA required
      for contributions. The CLA means contributions are licensed/assigned
      back to Future Present Labs, so — as full copyright holder — the
      company isn't bound by AGPL's terms the way an external fork would be,
      and can dual-license the same codebase commercially for the paid tier
      (validation libraries, official distributor integrations) without
      splitting into a separate closed repo. Same pattern GitLab/MongoDB/
      Mattermost use: AGPL protects the community edition from silent
      competing forks, CLA preserves the company's ability to sell a
      proprietary version of the same code. No repo split needed.
