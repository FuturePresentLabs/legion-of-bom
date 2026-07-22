---
name: lob-cli
description: >
  Work with the legion-of-bom pipeline (lob CLI) and its Phase 0 stages.
  Covers circuit definition, simulation, validation, BOM generation, and
  toolchain setup. Trigger when the user asks about lob, the pipeline,
  circuit-as-code, SKiDL, ngspice simulation, BOM generation, or any
  legion-of-bom workflow.
triggers:
  - "lob"
  - "legion-of-bom"
  - "pipeline"
  - "circuit-as-code"
  - "skidl"
  - "ngspice"
  - "bom generation"
  - "run the pipeline"
  - "verify circuit"
  - "simulate circuit"
---

# lob CLI & Pipeline Workflow

legion-of-bom is a circuit-as-code pipeline: SKiDL script in, manufacturing-ready
outputs out. The `lob` CLI is the local-first interface.

## Quick Commands

```bash
# Check toolchain (ngspice, SKiDL, KiCad symbols)
lob doctor

# Run the full Phase 0 pipeline on a circuit
lob run examples/rc_lowpass.py

# Fast feedback loop (Rust gates, cheapest first)
cargo check
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Phase 0 Pipeline Stages

`lob run <circuit>` executes these stages in order, exiting non-zero on failure:

| Stage | What it does | External tool | Fails if |
|---|---|---|---|
| `skidl` | Runs the SKiDL script, generates netlist + ERC | Python/SKiDL | Script errors, ERC errors |
| `parse` | Reads the netlist into the internal `Circuit` model | — | Malformed netlist |
| `validate` | Surfaces ERC results as structured findings | — | ERC errors (warnings pass) |
| `simulate` | Runs ngspice AC sweep, extracts passband/cutoff | ngspice | ngspice not found, sim diverges |
| `verify` | Checks simulated cutoff against textbook formula | — | Deviation > tolerance (default 2%) |
| `bom` | Groups parts, writes CSV, reports missing footprints | — | — (warnings only) |

**Output:** `out/<circuit_stem>/` containing netlist, SPICE deck, simulation results, and BOM CSV.

## Circuit Definition (SKiDL)

Circuits are Python scripts using SKiDL's `Part`/`Net`/`Pin` API. The pipeline reads
the generated KiCad netlist, not the Python directly.

**Minimal example:**
```python
from skidl import Net, Part, generate_netlist

r1 = Part("Device", "R", value="1k", footprint="Resistor_SMD:R_0805_2012Metric")
c1 = Part("Device", "C", value="159n", footprint="Capacitor_SMD:C_0805_2012Metric")

vin = Net("IN")
vout = Net("OUT")
gnd = Net("GND")

vin & r1 & vout & c1 & gnd
generate_netlist()
```

**Requirements:**
- `KICAD9_SYMBOL_DIR` env var pointing at KiCad symbol libraries
- Python venv with SKiDL installed (see `requirements.txt`)

## The `CircuitSource` Trait

All pipeline stages consume circuits through the `CircuitSource` trait (DESIGN.md
2.3/3.3), not SKiDL-specific types. Today the only impl parses SKiDL-generated
netlists; a future native DSL or IR is one new impl, not a rewrite.

```rust
pub trait CircuitSource {
    fn parts(&self) -> &[Part];
    fn nets(&self) -> &[Net];
    // ...
}
```

## Toolchain Setup

Run `lob doctor` to verify. Required for Phase 0:

| Tool | Discovery | Setup |
|---|---|---|
| ngspice | `PATH` | `brew install ngspice` (macOS) |
| SKiDL (Python) | `.venv/bin/python`, then `python3` | `python3 -m venv .venv && .venv/bin/pip install -r requirements.txt` |
| KiCad symbol libs | `KICAD9_SYMBOL_DIR`, then macOS default | `export KICAD9_SYMBOL_DIR="/Applications/KiCad/.../symbols"` |
| kicad-cli | `PATH`, then macOS app bundle | Optional at Phase 0 |

## Parts Verification Gate (MCP.md §1)

**Critical:** Before `layout` or `generate_bom` can run, every part must have
`verified_by_human = TRUE` in the global Dolt-backed parts library. This is the
structural fix for the LM13700 pinout-from-memory mistake.

- Parts are resolved by MPN against the global library
- `fetch_datasheet` retrieves structured pin/rating data (CAD library first, PDF fallback)
- Human verifies once per part — reusable across all future projects
- Unverified parts block layout and BOM generation

## Writing New Pipeline Stages

Stages implement the `Stage` trait:

```rust
impl Stage for MyStage {
    fn name(&self) -> &str { "my_stage" }
    fn run(&self, circuit: &dyn CircuitSource) -> Result<StageOutcome, StageError> {
        // ...
        Ok(StageOutcome::passed("my_stage")
            .with(Finding::info("something happened")))
    }
}
```

Rules:
- Return `StageError` if the stage **could not run** (missing tool, bad input)
- Return `StageOutcome::failed()` if it **ran but found problems**
- Never panic on missing tools — fail gracefully

## Common Issues

| Symptom | Cause | Fix |
|---|---|---|
| `SKiDL stage failed` | Missing venv or KiCad symbols | Run `lob doctor`, check `KICAD9_SYMBOL_DIR` |
| `simulate stage failed` | ngspice not on PATH | `brew install ngspice` |
| ERC warnings on IN/GND | Single-pin nets in two-port blocks | Expected — simulation adds the testbench |
| `no footprint` BOM warning | Part missing `footprint=` in SKiDL | Add footprint to `Part(...)` call |

## Conventions

- Circuit repos are plain git, one per project
- Inventory data lives in Dolt (`.beads/dolt/`), not git
- Each layout attempt is a natural git commit
- v1 is local-first, single-user, no auth
- The CLI and web backend wrap the same `legion-of-bom-core` library
