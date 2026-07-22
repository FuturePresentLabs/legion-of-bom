# Agent MCP Guide — legion-of-bom

Status: DRAFT. Companion to `MCP.md` — this is the practical "how to drive it"
reference for agents, not the architectural spec.

## What you can and cannot do

**You CAN:**
- Research topologies, search parts, fetch datasheets
- Define circuits in SKiDL (with verified part data only)
- Run validation, simulation, and BOM generation
- Submit a design package for human approval

**You CANNOT:**
- Place a real order. There is no `place_order` tool and never will be by default.
- Use pinout/rating data from model memory. Every fact must come from a fetched,
  cited source (CAD library or PDF datasheet).
- Skip the parts verification gate. `layout` and `generate_bom` block on
  unverified parts.

## The agent loop (concrete)

```
1. create_project("slew-limiter", PanelProfile::EurorackHP(8))
2. research_topology("slew limiter") → returns 2-3 reference circuits with citations
3. (human picks one)
4. For each part in the chosen topology:
   a. search_part("LM13700") → distributor records + datasheet URL
   b. fetch_datasheet("LM13700") → structured pin/ratings → global library
   c. (human verifies pinout once)
5. define_circuit(...) → writes SKiDL, referencing verified parts only
6. validate() → ERC
7. simulate() → ngspice AC sweep
8. layout() → gate passes (all parts verified), iterative loop runs
9. generate_bom() → real pricing/stock from JLCPCB/Mouser/DigiKey APIs
10. submit_for_approval() → packages everything for human review
11. (human places order manually)
```

## Tool reference

| Tool | When to use | Input | Output |
|---|---|---|---|
| `create_project` | Start a new board | name, `PanelProfile` | scaffolded repo |
| `research_topology` | Need a reference circuit | function name (e.g. "crossfader") | candidate topologies + citations |
| `search_part` | Find a part or substitute | MPN or functional description | distributor records, datasheet URL, stock/price |
| `fetch_datasheet` | Get pinout/ratings for a new part | MPN | structured data in global parts library |
| `define_circuit` | Write/update the circuit | SKiDL definition (verified parts only) | updated circuit file |
| `validate` | Check ERC | — | `StageOutcome` with findings |
| `simulate` | Run SPICE | sim config (optional) | `AcResult` with points, passband, cutoff |
| `layout` | Generate board layout | — | KiCad board file (gate: all parts verified) |
| `generate_bom` | Generate BOM with live pricing | — | BOM CSV + CPL (gate: all parts verified) |
| `submit_for_approval` | Package for human review | — | summary with schematic, sim, layout, cost |

## The verification gate (what actually blocks)

Before `layout` or `generate_bom`:

1. **Every used part** must have `verified_by_human = TRUE` in the global Dolt
   parts library (`parts` table).
2. **The circuit's open-questions checklist** must have zero unresolved items.

If blocked, the tool returns the specific blocking items. Your job:
- Resolve them (re-fetch, fix the circuit, ask the human to verify)
- Or surface them to the human and stop

**Never proceed past a blocked gate.** The gate is structural, not advisory.

## Parts library schema (what you are writing to)

```sql
-- Global, Dolt-backed, cross-project
CREATE TABLE parts (
  id INT PRIMARY KEY,
  mpn VARCHAR(64) NOT NULL,          -- canonical key
  manufacturer VARCHAR(128),
  datasheet_url TEXT NOT NULL,       -- from distributor API only
  fetched_at DATETIME,
  verified_by_human BOOLEAN DEFAULT FALSE,
  verified_at DATETIME,
  verified_by VARCHAR(64)
);

CREATE TABLE part_pins (
  part_id INT REFERENCES parts(id),
  pin_number VARCHAR(8),
  pin_name VARCHAR(64),
  cited_page INT                     -- traceable to source
);

CREATE TABLE part_ratings (
  part_id INT REFERENCES parts(id),
  rating_name VARCHAR(64),
  value TEXT,
  unit VARCHAR(16),
  cited_page INT
);
```

## Trust order for part data

When `fetch_datasheet` runs, it tries sources in this order:

1. **Distributor-official CAD library** (DigiKey KiCad library, JLCPCB/EasyEDA)
2. **Broader CAD library** (SnapEDA, Ultra Librarian, SamacSys) — check verification badge
3. **PDF datasheet** — sourced from distributor API URL field, never general web search

For pin assignments and absolute max ratings, attempt **two independent sources**
and flag mismatches rather than silently picking one.

## What to put in Beads (not markdown TODOs)

- Discovered follow-up work ("need to verify TL072 pinout")
- Blockers ("JLCPCB API rate limit hit")
- Phase decisions ("Phase 0 scope: RC filter + op-amp gain stage")
- Anything another agent or human should resume later

Use `bd create` for new tasks, `bd update --claim` to own work, `bd close` when done.

## Common pitfalls

| Pitfall | Why it happens | Fix |
|---|---|---|
| "LM13700 pin 16 is Iabc" from memory | Model hallucination | Always `fetch_datasheet` first, never trust memory |
| Layout blocked on unverified part | Forgot human verification step | Surface the part to human, wait for `verified_by_human = TRUE` |
| Two sources disagree on pinout | Cross-check found mismatch | Flag it explicitly, don't pick one silently |
| Circuit has unresolved open questions | Checklist item not checked off | Review the checklist in the circuit repo |

## Example: verifying a part

```
Agent: search_part("TL072")
       → { mpn: "TL072CDR", datasheet_url: "https://...", stock: 15000, price: 0.42 }

Agent: fetch_datasheet("TL072CDR")
       → extracted pins: [1: OUT1, 2: IN1-, 3: IN1+, ...]
       → extracted ratings: [Vcc_max: ±18V, Iout: 40mA, ...]
       → all cited to datasheet pages 3-4

Agent: (presents to human)
       "TL072CDR pinout extracted from DigiKey KiCad library (verified badge)
        and cross-checked against ST datasheet page 3. Confirm?"

Human: "Confirmed."
       → verified_by_human = TRUE

Agent: define_circuit(...)  // can now reference TL072CDR safely
```
