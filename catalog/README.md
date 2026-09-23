# The parts catalog

Known-good parts that `lob spec board` synthesizes circuits from — **data, not
code**. One JSON file per part, named `<mpn>.json`, reviewed in git like code.
The schema is `crates/core/src/catalog.rs`; the existing files are the worked
examples (`STM32H743VIT6.json` for an MCU, `PCM1808PWR.json` for a part with no
KiCad symbol, `WM8731SEDS.json` for figure-only facts, `PinHeader_*` for
connectors).

## The one rule

**Every fact is cited or it is a reading.** A value, strap, pin mapping or
support part is either a `{"page": N, "quote": "…"}` that appears **verbatim**
on page N of the part's pinned datasheet, or a `{"reading": "…"}` that a person
must confirm before the board is fab-ready. Nothing typed from memory.
`lob catalog check` holds every quote to its page and every pin name and
alternate to the KiCad symbol; a part is not done until it reports 0 problems.

## Adding a part

1. **Find it at a distributor.** EasyEDA's product search gives the LCSC code:
   `https://easyeda.com/api/eda/product/list?keyword=<MPN>&page=1&pageSize=5`
   (browser User-Agent, `Referer: https://easyeda.com/`).
2. **Pin the datasheet.** `https://www.lcsc.com/datasheet/<LCSC code>.pdf` is a
   page that embeds the real PDF link (`https://datasheet.lcsc.com/datasheet/pdf/<hash>.pdf`).
   Download it, take its SHA-256, and copy it to the datasheet store as
   `~/.local/share/legion-of-bom/datasheets/<sha256>.pdf`. Record `url` +
   `sha256` in the part's `datasheet`. The store is durable: Research-Wing
   ingests these PDFs later.
3. **Read it.** `pdftotext -layout part.pdf part.txt`; pages are separated by
   form feeds, and **page N means the Nth form-feed-separated page**, not the
   printed page label (`awk 'BEGIN{RS="\f"} /text/ {print NR}' part.txt`).
   A symbol font can render µ as `m` (TI) or as an invisible private-use
   character (Wolfson/Cirrus); quote what the page says — the checker folds µ,
   Ω, dashes and whitespace.
4. **Symbol.** Prefer an official KiCad symbol (`"symbol": {"kicad": "Lib:Name"}`);
   pin names are then the symbol's. With no KiCad symbol, give the pin table
   inline, one cited row per pin.
5. **Write the file**, then `lob catalog check` until it reports 0 problems.

## Roles (`provides`)

What synthesis asks for. Use these; add a new one only with its first user.

| role | meaning |
|---|---|
| `mcu` | a microcontroller synthesis binds buses to |
| `i2s-dac`, `i2s-adc` | audio converter on an I2S bus |
| `mic-in`, `headphone-out` | the part has that analog port |
| `rail:+3V3`, `rail:+1V8`, … | supplies that rail (and ties its input to another) |
| `crystal` | a crystal with `params.freq_hz` and `params.cl_pf` |
| `connector` | any connector; its `interfaces` say what it carries |
| `radio-subghz`, `radio-2g4` | an RF transceiver in that band |
| `clock-gen` | a programmable clock generator |
| `esd-usb` | ESD protection for a USB data pair |
| `flash-spi` | SPI NOR flash |
| `opamp` | general op-amp |

## Interface kinds

Signals are named from the part's own side. A slave's `din` is the data it
receives. An MCU maps each signal to an **alternate function**
(`{"alt": "SPI1_SCK"}`), and synthesis finds the pin from the symbol.

| kind | roles | signals |
|---|---|---|
| `i2s` | `master` / `slave` | `mclk`, `bck`, `lrck`, `din`, `dout` |
| `i2c` | `master` / `slave` | `scl`, `sda` |
| `spi` | `master` / `slave` | `sck`, `mosi`, `miso`, `cs` (slave: `mosi` = data in) |
| `gpio` | `slave` | named control lines to any free MCU pin: `reset`, `busy`, `irq`, … |
| `swd` | `target` / `debugger` | `swdio`, `swclk`, `nrst` |
| `hse` | `needs` (MCU) / `source` (crystal) | `in`, `out` |
| `rf` | `source` / `sink` / `port` | `rf` — a 50 Ω single-ended RF port |
| `usb` | `device` / `port` | `dp`, `dm` |
| `power-in` | `port` | `v` — the board's supply input |
| `stereo-audio`, `mono-audio` | `port` | `l`, `r` / `sig` |
| `audio-line-in`, `audio-line-out`, `audio-mic-in`, `headphone-out` | `sink` / `source` | `l`, `r` / `mic` |

## Needs

An interface with role `needs` (an MCU's or a radio's crystal, `hse`) is
filled per part: synthesis offers every part with a matching `source`
interface that fits the needer. A needer narrows that with cited `params`:

| param | meaning |
|---|---|
| `xtal_freq_hz` | only a crystal with this `freq_hz` fits (a radio's 32 MHz) |
| `xtal_min_hz`, `xtal_max_hz` | only a crystal in this range fits (an MCU's HSE, 4–26 MHz) |
| `xtal_load_internal` | `1` when the chip trims its crystal load internally: no external load caps |

Without `xtal_load_internal`, synthesis adds the load caps computed from the
crystal's own cited `cl_pf`.

## Subcircuits

A circuit around several parts — a radio with its RF switch and matching
network, an op-amp stage — is a **subcircuit**: one file under
`catalog/subcircuits/<name>.json`, beside `parts/`. It is a reference design
over role **slots**, not a board, and it is held to the same rule: every fact is
quoted from its pinned `source` (the reference design, app note or module
schematic) or is a reading.

```json
{
  "name": "sx1262-frontend-915",
  "summary": "what a decider is told",
  "source": {"url": "…", "sha256": "…"},
  "provides": ["radio-subghz"],
  "slots": {
    "radio":  {"provides": "lora-transceiver", "any_of": ["SX1262IMLTRT"]},
    "switch": {"provides": "rf-spdt"}
  },
  "interfaces": [{"kind": "rf", "role": "source", "signals": {"rf": "switch.pin:RFC"}}],
  "support": [
    {"between": ["radio.pin:DIO2", "switch.pin:CTRL"], "part": "tie", "cite": {…}},
    {"between": ["radio.pin:RFO", "node:tx"], "part": "L", "value": "…", "cite": {…}}
  ]
}
```

- **Slots.** A slot is filled from the parts that `provides` its role, and
  with `any_of` only those parts, for when the values are tuned to them (a
  PA match). One candidate is derived; several are a typed decision
  (`sub:<name>:<slot>` in the spec). Each member keeps its own support and
  interfaces: its bus, control lines and crystal are wired as usual.
- **Endpoints** are `slot.pin:NAME` (checked against the symbol of *every*
  part that could fill the slot), or the subcircuit's own `node:` / `net:`.
  It has no pins of its own, and no alternates or `each_pin`.
- **Interfaces** are what it exports. A feature's port is wired to a chosen
  subcircuit's export before any part's own interface.
- A part that is not a working function on its own (a radio die with no
  front end) should not `provide` the board-level role. The subcircuit does.

## Form factors

What shape a board is — a standard's outline and mounting holes, or a free
outline with holes in the corners — is one file under
`catalog/formfactors/<name>.json`, and which one a board gets is a typed
decision from its brief (`form_factor` in the spec).

```json
{
  "name": "rpi-hat",
  "summary": "what a decider is told",
  "source": {"url": "…", "sha256": "…"},
  "outline": {"width_mm": 65.0, "height_mm": 56.5, "cite": {…}},
  "holes": {
    "footprint": "MountingHole:MountingHole_2.7mm_M2.5",
    "at": [{"x_mm": 3.5, "y_mm": 4.0, "cite": {…}}]
  }
}
```

- **Coordinates** are board-local mm from the **top-left**, y down (KiCad's
  sense). Most drawings are dimensioned from the bottom left: convert, and
  say so in the cite.
- **No `outline`** means the board is sized to its parts.
- **Holes** are one KiCad footprint, which *is* the hole's geometry and
  keep-out (drill, courtyard): either `"corners": true` (four, each inset so
  its keep-out stays on the board, moving with the corners as the board is
  sized) or `at` the standard's points. They become parts `H1…`, placed and
  routed around like any other.
- `lob schematic` writes the chosen form factor as `<stem>.frame.toml`
  beside the circuit; `lob board` lays the board out in it.

## Support entries

Every tie, capacitor and resistor the part needs, between two endpoints —
`pin:NAME`, `net:NAME` (a board rail: `+3V3`, `+5V`, `GND`) or `node:NAME` (a
part-local node, e.g. the far side of a series resistor). `part` is `tie`, `C`,
`CP` (polarised), `L` or `R`; `each_pin: true` repeats the entry for every pin
carrying that name ("one 100 nF per VDD pin"). A matching network, a load cap,
a pull-up all live here, each cited.
