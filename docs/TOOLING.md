# Toolchain (Phase 0)

The Phase 0 pipeline shells out to external tools. This is the pinned, verified
set the loop runs against, and how `lob` locates each one. Run `lob doctor` to
check your machine against this list.

| Tool | Role | Required? | Version verified | Discovery |
|------|------|-----------|------------------|-----------|
| **ngspice** | Simulation (AC sweep) — DESIGN.md §5 | Yes (loop is sim-verified) | 45.2 | `PATH` |
| **SKiDL** (Python) | Circuit definition → netlist + ERC — DESIGN.md §3–4 | Yes | 2.2.3 | `.venv/bin/python`, else `python3` on `PATH` |
| **KiCad symbol libs** | SKiDL resolves `Device:R` etc. against these | Yes | KiCad 10 libs | `KICAD9_SYMBOL_DIR`, else macOS default path |
| **kicad-cli** | Layout / board output — DESIGN.md §6 | No (Phase 0 stretch) | 10.0.0 | `PATH`, else macOS app bundle |

## How `lob` finds tools

`PATH` first, then a known fallback (the macOS KiCad app bundle for `kicad-cli`).
The tool table lives in `legion-of-bom-core::tools` so the CLI, the future MCP
server, and the pipeline stages all discover tools the same way. Stages that
can't find a required tool fail gracefully — they never panic.

## Notes on the pinned versions

- **SKiDL 2.2.3 defaults to the `kicad9` backend**, so it reads symbol libraries
  from `KICAD9_SYMBOL_DIR` specifically (not `KICAD_SYMBOL_DIR` or `KICAD8_*`).
  KiCad 10's `.kicad_sym` format is compatible, so pointing `KICAD9_SYMBOL_DIR`
  at KiCad 10's libraries works.
- `kicad-cli` is **not on `PATH`** in a default macOS KiCad install; it lives at
  `/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli`. `lob` falls back to
  that path automatically.
- The RC demo emits **0 ERC errors**. It does produce 2 ERC *warnings*
  (single-pin nets on the filter's open `IN`/`GND` ports) — expected for a
  two-port block; the simulation stage adds the AC source/ground testbench.

## Setup

```bash
# 1. Python venv with SKiDL (pinned in requirements.txt)
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt

# 2. Point SKiDL at KiCad's symbol libraries (macOS default shown)
export KICAD9_SYMBOL_DIR="/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols"

# 3. ngspice (macOS)
brew install ngspice

# 4. Verify
cargo run -p legion-of-bom-cli -- doctor
```
