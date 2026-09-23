# Credits & Licenses — vendored component assets

`assets/` vendors a real, sourced set of panel-component footprints and 3D
meshes for hardware KiCad's own stock libraries don't cover well (Eurorack-
style 3.5mm jacks, 9mm pots, panel LEDs, slide switches). Both upstream
sources are license-clean for redistribution; this file records attribution
and license terms so the set can be re-fetched or extended later without
re-deriving provenance from scratch.

Brought in via the same vendoring this project's other author (Avery Wagar)
already did once for a private, unpublished workspace — re-vendored here
directly from the original upstream projects' own terms, not from that
workspace (which carries its own, separate licensing and is not a source of
truth for reuse).

## Panel footprints — 4ms-kicad-lib (Unlicense / public domain)

`assets/footprints/Eurorack_4ms.pretty/*.kicad_mod` are from **4ms-kicad-lib**
(`footprints-legacy/4ms-legacy-footprints.pretty`), released under the
**Unlicense** (public domain dedication). No attribution required; recorded
here for provenance.

Vendored footprints:

- `PJ301M-12.kicad_mod` — Thonkiconn / PJ301BM 3.5mm jack socket
- `POT-9MM-ALPHA.kicad_mod` — Alpha 9mm vertical pot, smooth shaft
- `POT-9MM-KNURL.kicad_mod` — Alpha 9mm vertical pot, knurled shaft
- `LED-3MM-SQUARE-ANODE.kicad_mod` — 3mm square LED
- `Slide_Switch_SS22D06-G6-H_Runrun.kicad_mod` — SS22D06 slide switch
- `RGB_ROTARY_ENCODER.kicad_mod` — RGB rotary encoder
- `POT-SLIDER-LED-ALPHA-RA2045F-20.kicad_mod` — Alpha RA2045F 20mm slide pot w/ LED

To re-fetch: `https://github.com/4ms/4ms-kicad-lib`, path
`footprints-legacy/4ms-legacy-footprints.pretty/`.

## 3D meshes — jolin-components (CC-BY 4.0) — ATTRIBUTION REQUIRED

`assets/meshes/jolin/{glb,step}/*` are from the **jolin-components** library
by **jolin**, licensed **Creative Commons Attribution 4.0 (CC-BY 4.0)**.

> 3D models © jolin (jolin-components), licensed under CC-BY 4.0.
> https://creativecommons.org/licenses/by/4.0/

Vendored meshes:

| File (stem) | Part |
|---|---|
| `jack_socket_PJ301BM_3.5mm` | Thonkiconn / PJ301BM 3.5mm jack socket |
| `pot_RD901F_6.35mm_shaft_alpha_9mm_vertical` | Alpha RD901F 9mm vertical pot, 6.35mm shaft |
| `pot_RD901F_T18_shaft_alpha_9mm_vertical` | Alpha RD901F 9mm vertical pot, T18 splined shaft |
| `knob_KN8F_6.35mm_shaft` | KN8F knob cap, 6.35mm shaft |
| `knob_RB671_Rogan_6.35mm_shaft` | Rogan RB671 knob cap, 6.35mm shaft |
| `LED_square_3mm_red` | 3mm square LED |
| `switch_slide_onoff_SS12D00` | SS12D00 slide switch |
| `fader_Bourns_45mm` | Bourns 45mm fader |

To re-fetch: search for `jolin-components` (jolin); confirm the CC-BY 4.0
terms are unchanged before re-vendoring, since attribution license terms can
be revised by the upstream author.

## Real Eurorack physical constants (public spec, not third-party IP)

Standard Eurorack dimensions, for reference against
[`crate::panel::BuiltinCutouts`](../crates/core/src/panel.rs) and
[`crate::pedal_panel::PedalCutouts`](../crates/core/src/pedal_panel.rs):

- `HP_MM = 5.08` — one horizontal pitch unit
- `EURORACK_HEIGHT_MM = 128.5` — standard 3U panel height

These are industry-standard physical facts (Doepfer's own A-100 spec), not
anyone's IP — no license question, just recorded here since they're the
same numbers this vendored data was cross-validated against.

## Status

Vendored for provenance and future use; not yet wired into
[`BuiltinCutouts`](../crates/core/src/panel.rs)'s hole-size table or exposed
via [`house_footprint_dir()`](../crates/core/src/parts.rs) as a built-in
fallback footprint library. That integration is tracked as follow-up work,
not done here.

## Not vendored here, and why

Real per-module hardware data for specific Eurorack modules (e.g. Mutable
Instruments Plaits/Marbles panel layouts) is **not** part of this set. That
data is real and does exist (in a separate, unpublished circuits repo, kept
deliberately isolated there), but it's licensed CC-BY-SA-3.0 (share-alike)
from its own upstream (`pichenettes/eurorack`) — folding share-alike-licensed
content into this AGPL-3.0-or-later repo would require this repo (or at
least the affected files) to carry compatible share-alike terms, which is a
real licensing decision, not a mechanical copy. If real per-module panel
data is wanted here later, follow the same isolation pattern (its own
directory, its own attribution file, explicit upstream credit) rather than
merging it into this general-purpose asset set.
