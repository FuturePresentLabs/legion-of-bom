# legion-of-bom

*SKiDLs are better with friends.*

**legion-of-bom** turns circuit-as-code into manufacturing-ready outputs and live
inventory/reorder state — one pipeline instead of KiCad + spreadsheets + manual
ordering scattered across tools. Starting with [Puget Audio](https://pugetaudio.com)
Eurorack boards.

Status: **early scaffold.** The pipeline is being proven on textbook circuits
before any real product design runs through it. See [`DESIGN.md`](./DESIGN.md) for
the full design and [beads](#issue-tracking) for the live task graph.

## The pipeline loop

A circuit is defined as code (SKiDL today), then flows through composable stages:

```
SKiDL script → netlist → parse → validate (ERC) → simulate (ngspice) → verify → BOM
                                                                          ↓
                                          (later) layout · panels · gerbers · PCBA · inventory
```

Phase 0's goal is to run that whole loop locally on a known-good RC low-pass filter
and a non-inverting op-amp gain stage — checking the simulation against textbook
values — so the loop is trustworthy before a real board depends on it.

## Architecture

- **`legion-of-bom-core`** (`crates/core`) — the pipeline library. Circuit model,
  the `CircuitSource` trait every stage reads through, `Stage` traits, and the
  report types. Deliberately DSL-agnostic (DESIGN.md 2.3/3.3).
- **`lob`** (`crates/cli`) — the command-line interface, a thin wrapper over the
  core library.
- **MCP server** — next interface after the CLI, over the same core library.
- **Web backend / UI** — deferred (axum + Slint/React later); nothing is web-only.

## Quickstart (dev loop)

Requires a Rust toolchain. Run the gates in this order — cheapest feedback first:

```bash
cargo check          # fast type/borrow check
cargo test           # unit tests
cargo build          # produce the lob binary

cargo fmt --check    # formatting (run often)
cargo clippy --all-targets --all-features -- -D warnings   # lints (run often)
```

Run the CLI:

```bash
cargo run -p legion-of-bom-cli -- run examples/rc_lowpass.py
# or, after `cargo build`:
./target/debug/lob run <circuit>
```

The Phase 0 pipeline stages (SKiDL runner, netlist parser, ngspice, BOM) shell out
to external tools — SKiDL/Python, ngspice, and KiCad — which are being pinned in
the Phase 0 tooling task before the runner lands.

## Issue tracking

This project tracks work in **[beads](https://github.com/gastownhall/beads)** (`bd`),
not markdown TODOs. The roadmap (DESIGN.md §14) is modeled as epics per phase:

```bash
bd ready              # what's available to work on now
bd list --type=epic   # the phase roadmap
bd show <id>          # details + dependencies
```

## License

[AGPL-3.0-or-later](./LICENSE).
