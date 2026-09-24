# Handoff: Prompt to PCB — synthesis, subcircuits, form factors

*Written 2026-09-23 at the end of a long session on branch
`catalog/known-good-parts`. Read this, then `DESIGN.md`, `catalog/README.md`
and `~/.claude/CLAUDE.md` (the FPL tenets).*

## What we are building

legion-of-bom is a **tool that designs boards**, not a set of boards. The loop:

1. **Requirements as typed decisions.** RLCD / System One, through the
   `ooda` crate, decides from a free-text brief.
2. **Catalog-driven synthesis.** Every part, subcircuit and form factor
   comes from hand-curated, cited JSON under `catalog/`.
3. **SKiDL as the IR.** A pure function of the decisions.
4. **Typed decisions again on the layout.** Only partly built so far.

Never hand-author a board for the user. If a board needs something the
catalog can't express, extend the data or the schema, not a template.
`mcu_audio.rs` was exactly that mistake, and it was deleted.

The targets, in order, all meant to become PCBBench tasks:

1. A small STM32 audio-codec board (H7, all three codec options).
2. A small desktop synth.
3. An Android-class feature phone (the north star).

RF work is underway: sub-GHz radios now synthesize end to end.

## State of the branch

- **Branch:** `catalog/known-good-parts`. It has **14 commits not pushed**
  (`bd9fa5a` … `3c6a11b`) and no PR yet. Pushing and opening a PR need the
  user's go-ahead.
- **Uncommitted files:**
  - `.beads/issues.jsonl` — noise from the old beads tracker; not ours to
    commit.
  - `catalog/staging-rf2/` — the old SX1262 draft with `"TBD"` values. It is
    superseded by `catalog/subcircuits/sx1262-frontend-915.json` and can go
    once someone agrees.
  - `catalog/staging-sx1262/`:
    - `frontend-sources.md` — the full source trail for the SX1262 front-end
      values.
    - `gen.jq` — the generator for the subcircuit JSON.
    - `PE4259-63.json` — already merged into `catalog/parts`.
  - `catalog/staging-formfactors/rpi-hat-sources.md` — the pinned Raspberry
    Pi HAT / HAT+ drawings with every dimension. Needed for `wbr4`.
- **Gates at `3c6a11b`:** `cargo fmt --check` and clippy with `-D warnings`
  are clean. `cargo test` passes 562 tests, 6 ignored; the ignored ones need
  KiCad symbols or datasheets. Run them with `--ignored` and they pass
  locally.
- **Catalog check:** `lob catalog check` reports 39 parts, 1 subcircuit,
  5 form factors, 388 quotes checked, 150 readings awaiting confirmation,
  0 problems.
- **PCBBench** (`~/src/rlcd/pcbbench`, `main` pushed at `703f2ec`):
  - The scorer counts a yes/no decision's confidence as max(p, 1−p).
  - `tasks/planned/` holds `board-stm32-codec-{headphone,mic,no-bus}-v1`
    and `board-subghz-telemetry-v1`, all on `family = "board"`.
  - None of these has been scored through `board` + `drc` yet.
  - The `feat/task-input` branch there holds `e2eaa44`. It was
    cherry-picked into `main`, so the branch is now redundant.

## How to run the loop

```bash
set -a; source .env; set +a
export OODA_MODEL=fpl/decide      # .env says fpl/semif, which 502s; do NOT edit .env
lob spec board --brief "…" --out /tmp/x/node --trace /tmp/x/trace.json
lob schematic /tmp/x/node.json --out /tmp/x/node.py   # also writes node.frame.toml
lob run /tmp/x/node.py                                 # SKiDL → ERC → sim gating → BOM
cargo run --release -p legion-of-bom-cli -- board /tmp/x/node.py   # layout: release build only
lob catalog check        # every quote on its page, every pin in its symbol, every footprint exists
lob catalog list
```

These briefs pass `lob run` live:

| Brief | Result |
|---|---|
| PCM5102A + PCM1808 on an H7 | 48 parts |
| WM8731 mic | 57 parts |
| ES8388 headphone | 48 parts |
| 915 MHz CC1101 node (G0B1) | 36 parts, 11 decisions, ~35 s total |
| LoRa node on the `sx1262-frontend-915` subcircuit (F411) | passes |
| CC1101 node on M3 standoffs | `form_factor` chosen at 1.00; `lob run` not re-checked on this spec |

## Architecture

### Data in `catalog/`, reviewed in git like code

The guide is `catalog/README.md`.

- **`parts/<mpn>.json`** — `CatalogPart`: symbol (KiCad or an inline pin
  table), footprint, datasheet URL + sha256, `provides` (roles), `params`,
  `interfaces` (kind, role, signals mapped to `pin:` / `node:` / `{alt}`),
  and `support` (ties and C/CP/L/R between endpoints).
- **The one rule:** every fact is a `{"page", "quote"}` that is verbatim on
  the Nth form-feed page of `pdftotext -layout`, or a `{"reading"}` a person
  must confirm.
- **`subcircuits/<name>.json`** — `Subcircuit`: a reference design over role
  **slots** (`provides`, optional `any_of`). Its endpoints can be
  `slot.pin:X`, `slot.node:X`, or its own `node:` / `net:`. It exports
  interfaces and carries its own cited support.
- **`formfactors/<name>.json`** — `FormFactor`: an optional fixed outline,
  plus holes given as one KiCad `MountingHole:*` footprint placed either in
  the `corners` or `at` points. Coordinates are measured from the **top
  left, y down**.
- **`features.json`** — the requirement vocabulary: key, question, role,
  interface, port, net.
- **Fingerprint:** a hash over parts, subcircuits, form factors and
  features, pinned in every spec. A stale spec is refused.
- **Datasheet store:** `~/.local/share/legion-of-bom/datasheets/<sha256>.pdf`.
  It is durable (Research-Wing will ingest it). **Never prune it.**

### Code in `crates/core/src`

- **`catalog.rs`** — the types, the loader (file name must equal
  name/MPN), shape checks, and the `check_symbols`, `check_quotes` and
  `check_footprints` checkers.
- **`synth.rs`** — `design()` runs these decisions in order:
  1. Requirements: one yes/no decision per feature.
  2. `form_factor`.
  3. `function`: minimal covers of the required roles over `Unit`s (a part
     or a subcircuit).
  4. Subcircuit slots: `sub:<name>:<slot>`.
  5. `mcu`: must have a master for every bus.
  6. `needs:<kind>:<mpn>`: crystals, filtered by `fits_need`
     (`xtal_freq_hz`, `xtal_min_hz`, `xtal_max_hz`).
  7. `rail:*`.
  8. `port:*`.
  9. `bind_<bus>`: options describe their pins and the package side.

  Each step uses `pick()`: one candidate is filled directly ("derived");
  several go to RLCD as a typed choice.

  `circuit()` is spec → `skidl_emit::Circuit`: placement, union-find nets,
  subcircuit expansion, hole parts `H1…`. `frame()` is spec → `BoardFrame`.
- **`frame.rs`** — `BoardFrame`: an outline or none, `pinned` points, and
  `corners`. Corner parts are inset by keep-out/2 + `EDGE_CLEARANCE_MM`.
- **`board.rs`** — `framed_template` and `minimum_framed_outline`: the
  square size search re-pins the frame's parts at every trial size.
  **It fails loud** if nothing up to 300 mm fits.
- **`family.rs`** — `Spec::Board(DesignSpec)`, plus `render_skidl`,
  `unconfirmed_facts`, `frame()` and `panel()`.
- **`datasheet.rs`** — fetch/pin, `pdftotext` pages, and `normalize`, which
  folds µ (including the Symbol-font U+F06D), Ω, dashes and whitespace.
- **`skidl_emit.rs`** — writes SKiDL that finds pins by name through a
  `pins()` helper that raises.

### CLI in `crates/cli/src/main.rs`

- `lob spec board` prints yes/no decisions as `yes|no (p=…) (confidence max(p,1-p))`.
- `lob schematic` writes `<stem>.frame.toml` and removes a stale one.
- `lob board`, `fab` and `guide` read `<stem>.frame.toml` beside the
  circuit, the same way they read `<stem>.placement.toml`. A board with
  both a panel and a frame is an error.

## Known problems and open findings

1. **Layout of the M3-standoff board has never been verified.**
   - The first run exposed the corner-inset bug, now fixed in `3c6a11b`: the
     board was being sized to 300 × 300 mm.
   - The re-run then ran for **3+ hours of release-build CPU** without
     finishing. Its output directory was the session scratchpad, which was
     deleted, so it was killed with no result.
   - **Next agent:** re-run `lob board` on a fresh `spec → schematic` for a
     small free-m3-corners board, in a durable directory. Check the outline
     size and the H1–H4 positions in the `.kicad_pcb`. Then find out why
     layout takes hours; compare with `y17.10` (router runtime at fine pitch)
     and `y17.11`.
2. **HAT is outline + 4 holes only** (`wbr4`). Still missing:
   - The 40-pin header pinned by the frame. Frame anchors are x/y only;
     they need rotation and side.
   - The Pi as the bus master, so a HAT board has no MCU.
   - The HAT+ ID EEPROM subcircuit.

   All HAT dimensions are readings; the drawing has no text layer.
3. **Synth hard-codes board assumptions** (`r6jn`): a +5 V supply through a
   power-in connector, exactly one MCU, SWD always wired, and `free_gpio`
   matching only `P[A-Z]n` pin names.
4. **Weak decisions** (`qg46`): the MCU and the 3.3 V regulator are picked
   at 0.12–0.48 confidence because the options carry too little. Next: the
   DESIGN §3.4 `requires` block, numeric slot constraints over cited params,
   so logic filters before RLCD picks.
5. **SX1262 front-end values are readings** that name rows in Semtech's
   reference BOM, which is an xlsx (`hu63` would make them checkable). The
   reference board is DC-DC; our SX1262 part is wired for its LDO. The
   PE4259 switch showed 0 stock at LCSC; stocked clones exist but are not
   curated.
6. **Outlines are rectangles only**, with no corner radius (the HAT's 3 mm
   radius is not modelled). The free-outline search tries **squares only**.
7. **`unconfirmed` over-reports** (`u7v2`): it lists every reading on a
   chosen part, not just the ones the design uses.
8. **ERC warnings:** the LoRa board shows 7. Not yet looked at.

## Suggested next steps, in order

1. Verify the form-factor layout, finding 1 above, and fix whatever it
   shows. Then close `rf6g` with its commit.
2. `wbr4`: widen frame anchors to rotation + side, pin the HAT header, add a
   host part for the Pi, and add the HAT+ EEPROM subcircuit. Then add a
   PCBBench HAT task.
3. The slot `requires` block (`qg46`); then `r6jn`, moving synth's
   assumptions into data.
4. More form factors from the marbles below, cheapest first: the free
   variants are done, then Feather, pHAT, mikroBUS, FC stacks. Each gets a
   pinned drawing and readings or quotes.
5. Score the PCBBench planned `board-*` tasks through `board` + `drc` and
   move them up into `tasks/`.
6. Enclosure contract for transmog (`wgi7`).

## Marbles (tracker: https://marbles.fpl.dev; `closed` needs merge evidence)

| id | what |
|---|---|
| uvdm | epic: synthesis loop |
| 3wbu | epic: form factors |
| rf6g | free outline + corner standoffs (implemented, layout unverified) |
| aze9 | Raspberry Pi HAT / HAT+ (outline + holes done) |
| wbr4 | HAT: Pi as host, 40-pin header pinned, HAT+ EEPROM |
| vpkp | pHAT / Zero |
| ydsx | Feather / FeatherWing |
| mc46 | Arduino Uno R3 shield |
| xjff | mikroBUS Click |
| fb52 | M.2 2230/2242/2280 |
| 6vmn | FC / ESC stacks |
| w7cq | Eurorack by HP (reconcile with `panel.rs`) |
| zpsk | Hammond 125B / 1590 (reconcile with `pedal_panel.rs`) |
| nun3 | PC/104 |
| wgi7 | enclosure contract for transmog |
| 2j96 | SX1262 front end: values cited as BOM readings, PE4259 added; remaining gap is cited quotes (see hu63) |
| qg46 | weak MCU and regulator decisions |
| hu63 | checker reads xlsx BOM rows |
| jev2 | noul confidence (fixed in lob + PCBBench; ooda contract unchanged by design) |
| u7v2 | unconfirmed over-reports |
| r6jn | synth assumptions into data |
| y17.10 / y17.11 | router runtime / pin-aware placement |

## User preferences (hard rules)

- **Build a tool, not boards.** Requirements → typed decisions → SKiDL IR →
  typed layout decisions.
- **Track work in marbles, not beads**, even though `CLAUDE.md` still says
  `bd`.
- **USB-C only.** Never add micro- or mini-USB parts.
- **Never prune the datasheet store.**
- **Rust first.** No Python for first-party code; Lua only for bounded
  plugins. Form-factor *dimensions* are data (JSON); *arrangement
  convention* is Lua (the `panel_lua.rs` split).
- **Commits:** local commits of verified work are authorized. Push, merge,
  PR or release only with an explicit go-ahead.
- **Keys and config:** never put API keys in a circuits repo. Don't edit
  the user's `.env`; override it through the environment.
- **Workflow tool** only on explicit opt-in. Use the Agent tool for
  subagents.
- **LLMs:** they may draft catalog entries (subcircuits, form factors) at
  curation time, validated by `lob catalog check`. At board-design time the
  loop stays typed decisions over the catalog; the LLM never invents slots
  there.

## Gotchas

- **Don't edit Rust with multi-line perl.** Brace delimiters and
  replacements containing `{}` break silently or half-apply. Use the Edit
  tool, or splice by line range.
- **`main.rs` has mis-encoded dashes** in a few strings (they print as
  `â`), so Edit tool matches on those lines fail. Splice by line number.
- **zsh doesn't word-split** `$var` in `for`/`set --`. Write files
  explicitly.
- **`ooda` is a path dependency** (`../../rlcd/ooda`). Another agent may be
  mid-commit there; a transient compile error from it usually clears on
  retry.
- **`lob board` in a debug build is unusably slow.** Use `--release`
  (target dir `/Volumes/DockBuild/pepalkernel_legion-of-bom-target`).
- **Never write run outputs to a session scratchpad** if anyone needs them
  later.
