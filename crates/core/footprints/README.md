# Bundled sub-board footprints

Real KiCad footprints for Daisy sub-boards, embedded so a circuit can reference
them via the `LobModule:` library (see `crates/core/src/subboard.rs`).

`Daisy-Boards.pretty/` is vendored from Electrosmith's **DaisyKiCad** library
(https://github.com/electro-smith/DaisyKiCad), MIT-licensed — see
`DaisyKiCad-LICENSE`. Pad names are the physical Daisy pin names (`A1`…`D10` /
`A1`…`E10`), so nets map straight to the datasheet pinout.

| Footprint | Module | Mount |
|---|---|---|
| `DAISY_PATCH_SM`      | Patch Submodule | through-hole 2×5 headers ×4 |
| `DAISY_PATCH_SM_SMT`  | Patch Submodule | SMT headers |
| `DAISY_SEED2_DFM`     | Seed2 DFM       | SMT (1.27 mm) female landing |
| `DAISY_SEED2_DFM_PTH` | Seed2 DFM       | through-hole (1.27 mm) female landing |

The Daisy Seed itself is synthesized in `subboard.rs` (not vendored here).
