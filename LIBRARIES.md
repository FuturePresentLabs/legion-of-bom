# legion-of-bom — Library Sources (LIBRARIES.md)

Status: DRAFT. Reference doc, not architecture — a curated registry of where
symbol/footprint/3D-model/SPICE data comes from, what's known about licensing,
and what tooling already exists to import from each source.

**Scope note**: license fields here are informational metadata surfaced to the
user, not a pipeline gate. `legion-of-bom` doesn't police how end users use a
library in their own designs — that's between them and the library source.
The one place this project has actual exposure is if `legion-of-bom` itself
bundles/redistributes library files as part of its own repo (see the
CC-BY-SA "collection" clause below) — that's a narrower, separate concern from
general usage tracking.

---

## 1. Distributor-official (highest trust, matches existing sourcing rules)

| Source | What it provides | License notes |
|---|---|---|
| **KiCad official libraries** (`kicad-symbols`, `kicad-footprints`) | Symbols, footprints, some 3D models. Includes community-contributed Eurorack parts already merged in (QingPu 3.5mm jacks, Alpha pots) | **Confirmed**: CC-BY-SA 4.0. Using the library in your own design does NOT require your design to be CC-BY-SA — only *redistributing the library itself as a collection* (including modified) requires same-license + attribution retained. Cleanest, best-documented license of anything on this list. |
| **DigiKey official KiCad library** | In-house, applications-engineering-built | License not confirmed in research — check directly before relying on specifics |
| **JLCPCB/EasyEDA library** (via `JLC2KICAD_lib` conversion) | Symbols/footprints/3D models tied to the JLCPCB Parts API already integrated | License not confirmed — check LCSC/EasyEDA terms directly |

## 2. Broad third-party providers (large catalogs, mixed trust)

| Source | What it provides | License notes |
|---|---|---|
| **SnapEDA** (now "SnapMagic Search") | Millions of parts, automated + manual verification, verification badge system | Free to use in your own designs per general commercial understanding — actual ToS for redistribution not confirmed here, check directly |
| **Ultra Librarian** | Similarly large catalog, many CAD tool integrations | Same caveat as SnapEDA |
| **SamacSys** (component search engine, now largely folded into Ultra Librarian's ecosystem) | Same category | Same caveat |

## 3. Eurorack-specific community libraries

| Source | What it provides | License notes |
|---|---|---|
| **`danroblewis/kicad-eurorack-tools`** | Not just parts — includes a pcbnew plugin that draws eurorack-spec'd panel cutouts pre-aligned for Alpha pots, Thonkiconn jacks, and LED holes. Directly relevant to Section 7 (Panel Design) | License not confirmed — check repo |
| **`30350n/protorack-kicad`** | Careful, manufacturer-spec or friction-fit footprints, every part has a matching high-quality 3D model. Pairs with `pcb2blender` for renders | License not confirmed — check repo |
| **`nathanaelnoir/KiCad-Eurorack-Library`** | Personal working library, validated against KiCad 9 | Has its own LICENSE.md — but explicitly flags that some 3D/STEP files weren't created by the repo owner and may need separate attribution/permission. **License is not uniform within this one repo** — check per-asset, not just per-repo |
| **`russellmcc/eurorack_kicad`** | Panel/footprint templates, referenced in community forum threads as a known-good starting point | License not confirmed |
| **`nebs/eurocad`** (and `wgd-modular/eurocad` fork) | Components/footprints used in the author's own Eurorack modules | License not confirmed |
| **`jjradler/eurorack-kicad-library`** | Hard-to-find parts for Eurorack development | License not confirmed |
| **`benjiaomodular` org** (`KiCadLibraries` repo + related) | Part of a broader ecosystem of actually-shipped open Eurorack module designs (includes a MiniVCA built around LM13700 — same part as the slew limiter) | License not confirmed — but the sibling shipped-module repos may be worth studying as reference designs too, separate from the library question |
| **`tristan-smith/HA_KicadLibraries`** | Common audio electronics/Eurorack footprints (jacks, pots, switches) | License not confirmed |
| **AudioMorphology's KiCad front panels repo** | Panel templates specifically (not a footprint library) | License not confirmed |

---

## 4. Import & library management tooling (don't reinvent these)

- **KiCad's built-in Plugin and Content Manager (PCM)** — several of the
  libraries above (e.g. `kicad-eurorack-tools`) ship a `repository.json`
  specifically so they install directly through PCM rather than manual file
  copying. Check for a PCM manifest before writing custom import code for any
  given source.
- **"Import-LIB-KiCad-Plugin"** — existing KiCad plugin that automates pulling
  downloads from SnapEDA, SamacSys, and Ultra Librarian into KiCad's library
  format, rather than manual per-part download-and-place.
- **`JLC2KICAD_lib`** — existing Python tool converting JLCPCB/EasyEDA library
  entries (schematic, footprint, 3D model) into native KiCad format. Natural
  fit given JLCPCB is already integrated for BOM/PCBA.
- **SamacSys Library Loader** — desktop tool for searching/pulling parts
  directly into a CAD tool's library path.
- **`KICAD_3RD_PARTY` environment variable convention** — community pattern
  for keeping t[118;1:3uhird-party library sources in clearly separated, trackable
  folders (e.g. one subfolder per source) rather than merging everything into
  one undifferentiated pile — useful convention for `legion-of-bom` to adopt
  when managing multiple sources with different trust/license status.

**Practical implication for `search_part`/`fetch_datasheet`**: prefer driving
these existing tools (PCM installs, `JLC2KICAD_lib`, the Import-LIB plugin)
over writing new scrapers/parsers against each source's website — several of
these sources already have purpose-built, community-maintained import paths
that `legion-of-bom-core` can shell out to or wrap, rather than duplicating.

---

## 5. Open item

- [ ] For every "license not confirmed" row above: a real license check
      (reading the repo's actual LICENSE file / provider's actual ToS) should
      happen before `legion-of-bom` treats any of these as a first-class,
      recommended source in documentation or tooling defaults — not before a
      person can use them, just before *we* vouch for them.
