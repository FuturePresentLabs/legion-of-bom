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
| `xtal_load_internal` | `1` when the chip trims its crystal load internally: no external load caps |

Without `xtal_load_internal`, synthesis adds the load caps computed from the
crystal's own cited `cl_pf`.

## Support entries

Every tie, capacitor and resistor the part needs, between two endpoints —
`pin:NAME`, `net:NAME` (a board rail: `+3V3`, `+5V`, `GND`) or `node:NAME` (a
part-local node, e.g. the far side of a series resistor). `part` is `tie`, `C`,
`CP` (polarised), `L` or `R`; `each_pin: true` repeats the entry for every pin
carrying that name ("one 100 nF per VDD pin"). A matching network, a load cap,
a pull-up all live here, each cited.
