<div align="center">

# legion-of-bom

### Prompt to PCB.

*One sentence in. A placed, routed, DRC-checked board out — and every value on it has a receipt.*

[![license](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](#license)
[![rust](https://img.shields.io/badge/built%20with-Rust-dea584.svg?logo=rust)](https://www.rust-lang.org)
[![kicad](https://img.shields.io/badge/KiCad-9-314cb0.svg)](https://www.kicad.org)
[![tests](https://img.shields.io/badge/tests-594%20passing-brightgreen.svg)](#dev-loop)
[![families](https://img.shields.io/badge/curated%20families-2-8a2be2.svg)](#the-pipeline)
[![datasheets](https://img.shields.io/badge/pinned%20datasheets-1-orange.svg)](#the-pipeline)
[![benchmarked](https://img.shields.io/badge/benchmarked%20by-PCBBench-black.svg)](https://github.com/FuturePresentLabs/pcbbench)

*SKiDLs are better with friends.*

</div>

```bash
lob spec board --brief "stereo line in and out, no I2C setup" \
  --model fpl/decide --out design
lob schematic design.json --out board.py
lob board board.py && lob drc out/board/board.kicad_pcb
```

`--model` is an opaque gateway model slug: it may select a conventional LLM
or an RLCD/System-One model. It overrides `OODA_MODEL` for that invocation,
which lets PCBBench compare models without mutating process-wide configuration.

Engineering profiles are explicit constraints, never claims inferred from a
brief. `--require-standard` puts an implemented profile into every RLCD request
and the replayable spec; `lob standards` then checks the produced circuit
deterministically:

```bash
lob spec board --brief "USB-C powered stereo codec" \
  --require-standard usb-type-c-2.0-sink --out design
lob schematic design.json --out circuit.py
lob standards circuit.py --require usb-type-c-2.0-sink --json standards.json
```

Reports distinguish artifact checks from proxies and physical tests. A USB-C
CC topology can pass while electrical/interoperability testing remains `TEST`;
Legion does not turn a design review into a certification claim. Run
`lob standards` without a circuit to see implemented and planned profiles.

That's a design brief becoming an STM32H743 audio board: a codec **decided**
from what you asked for, pins wired by the names in the official KiCad symbols,
every capacitor and strap **cited** to a page of a pinned datasheet, then placed,
routed and checked by KiCad's own DRC. No schematic drawn by hand, no pinout
typed from memory, no value you can't trace.

**legion-of-bom** turns circuit-as-code into manufacturing-ready outputs —
placed and routed boards, panels, Gerbers, a JLCPCB assembly package, build
guides and a priced BOM — in one pipeline instead of KiCad + spreadsheets +
manual ordering scattered across tools. It started with
[Puget Audio](https://pugetaudio.com) Eurorack modules and now designs
microcontroller boards too.

- 🎛️ **Ships real hardware.** The Puget Audio slew limiter builds from source to a
  DRC-clean, 0-unconnected fab package.
- 🧠 **Decides, doesn't hallucinate.** Every design choice is a bounded, typed
  decision — a choice, a probability, a rubric score — never free text. The
  rest is a pure function of those answers.
- 🧾 **Cites everything.** 0.5 mm pins escape through a router that measures
  clearance in exact geometry; decoupling values come with a verbatim datasheet
  quote a machine checks; anything it can't check waits for a human to sign.
- 📏 **Keeps score.** [PCBBench](https://github.com/FuturePresentLabs/pcbbench)
  runs brief → board → DRC against a rubric, so "does it work" has a number.

*(Badge numbers are generated — run `scripts/update-badges.sh` after tests,
families or datasheets change; don't hand-edit them.)* See
[`DESIGN.md`](./DESIGN.md) for the full design and [marbles](#issue-tracking)
for the live task graph.

## The pipeline

A circuit is defined as code (SKiDL), or decided from a design brief and then
rendered to code, and flows through composable stages:

```
brief ─(lob spec)─► spec ─(lob schematic)─► SKiDL circuit
                                              │
        lob run:  netlist → parse → ERC → simulate (ngspice) → verify → BOM
        lob board: place → legalize → route → .kicad_pcb → lob drc (kicad-cli)
        lob fab / guide / build: Gerbers + drill + CPL + BOM, assembly guide, Visual BOM
```

- **Typed decisions, curated families.** `lob spec <family> --brief "…"` asks a
  small number of bounded questions (a choice, a probability, a rubric score —
  never free text) of a System One–compatible endpoint through the shared
  [`ooda`](https://github.com/FuturePresentLabs/ooda) client, and writes a spec. `lob schematic` renders
  the circuit as a pure function of that spec. Families today: `fuzz-pedal`
  (plus `lob spec-chain` for chained gain stages) and `board`, which
  synthesizes the circuit: requirements, parts and bus bindings are each typed
  decisions whose options come from the [parts catalog](catalog/README.md), and
  the tool writes the SKiDL.
- **Cited, not remembered.** The catalog is hand-curated JSON, one file per
  part. Pinouts come from the official KiCad symbols, connected by pin name and
  checked against the symbol file (including pin-mux alternates). Every
  component value carries a verbatim quote from a
  page of a pinned datasheet — distributor copy, URL + SHA-256 — that is checked
  mechanically with `pdftotext`. Values read off a figure, or not stated in any
  pinned source, are recorded as readings; `lob schematic` lists every one no
  person has confirmed yet.
- **Layout for Eurorack and free boards.** A board with a panel is laid out
  against it (the PCB also derives its own minimum-width panel); a board without
  one gets a free rectangle sized to its parts. A negotiated-congestion router
  escapes 0.5 mm-pitch QFP/QFN pins and measures clearance in exact geometry.
- **Measured, not assumed.** Simulation checks textbook circuits against
  analytic values, `lob scope-probe` measures clipping as a SPICE claim, and
  [PCBBench](https://github.com/FuturePresentLabs/pcbbench) scores whole runs — brief to DRC — against a
  task rubric.

## Architecture

- **`legion-of-bom-core`** (`crates/core`) — the pipeline library: circuit model
  and the `CircuitSource` trait every stage reads through (DSL-agnostic, DESIGN.md
  2.3/3.3), placement, routing, rules, panels, fab, guides, SPICE, the curated
  families (`family.rs`) and datasheet citations (`datasheet.rs`).
- **`lob`** (`crates/cli`) — the command-line interface, a thin wrapper over the
  core library.
- **`legion-of-bom-web`** (`crates/web`) — the local read-only dashboard
  (`lob serve`) over the same core.
- **Parts library** — a global SQLite store (via SQLx) of part definitions keyed
  by MPN, with a `verified_by_human` gate on real board/BOM generation
  (`lob parts …`). Distributor clients (Mouser, JLCPCB) are sandboxed Lua scripts,
  not built-in Rust.

## Setup

Rust toolchain, plus the external tools the stages shell out to — each stage
fails with a clear error, never a panic, when its tool is missing:

- **KiCad 9** (`kicad-cli`, symbol + footprint libraries) — boards, DRC, fab
- **Python + SKiDL** in a `.venv` (or `$VIRTUAL_ENV`) — circuit scripts
- **ngspice** — simulation
- **poppler** (`pdftotext`) — datasheet citation checks

`lob doctor` reports what it found. API keys (`OODA_API_KEY` for `lob spec`,
`MOUSER_API_KEY`, `JLCPCB_*`) go in `~/.lob/credentials` or a repo `.env` — see
[`.env.example`](./.env.example).

## Quickstart

```bash
cargo run -p legion-of-bom-cli -- run examples/rc_lowpass.py        # simulate a textbook filter

lob spec board --brief "stereo line in/out, no I2C setup" --out design
lob schematic design.json --out board.py                            # + unconfirmed readings
lob run board.py && lob board board.py && lob drc out/board/board.kicad_pcb
```

## Dev loop

Run the gates in this order — cheapest feedback first:

```bash
cargo check          # fast type/borrow check (run first, run often)
cargo test           # unit tests
cargo build          # produce the lob binary

cargo fmt --check    # formatting
cargo clippy --all-targets --all-features -- -D warnings   # lints, warnings are errors

# Checks that need the installed KiCad libraries and the pinned datasheets:
cargo test -p legion-of-bom-core -- --ignored
```

## Vendored assets

`assets/` carries real, sourced third-party panel-component footprints and
3D meshes (Eurorack-style jacks, pots, LEDs, switches) for hardware KiCad's
own stock libraries don't cover well. See [`assets/CREDITS.md`](assets/CREDITS.md)
for exact provenance, licenses (Unlicense, CC-BY 4.0 — both redistribution-
clean), and where to re-fetch them.

## Issue tracking

Work is tracked in **[marbles](https://github.com/FuturePresentLabs/marbles)**
(hosted, one writer per project), not markdown TODOs. Ids carried over unchanged
from beads. The roadmap (DESIGN.md §14) is modeled as epics:

```bash
marbles ready                 # what's available to work on now
marbles list --json           # everything, machine-readable
marbles show <id> --json      # details + dependencies
```

Closed means merged: finished work moves to `review` with its PR and closes on
merge.

## License

[AGPL-3.0-or-later](./LICENSE).
