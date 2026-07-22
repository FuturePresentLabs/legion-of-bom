# Phase 0 Test Circuits — legion-of-bom

Purpose: two textbook circuits used to validate the full legion-of-bom pipeline
(SKiDL → ERC → ngspice sim → KiCad layout → BOM/CPL) against known-good, hand-
calculable answers before any real product design goes through it.
These are deliberately boring — the goal is a trustworthy answer key, not an
interesting circuit.

---

## Circuit 1: RC Low-Pass Filter

**Topology:** single-pole passive RC low-pass. Input → R1 → output node → C1 → GND.

**Components:**
| Ref | Part | Value | Notes |
|-----|------|-------|-------|
| R1  | Resistor | 1.6 kΩ | E24 standard value |
| C1  | Capacitor | 100 nF | C0G/film preferred for predictable sim |

**Target spec:**
- Cutoff frequency (−3dB point): `fc = 1 / (2π·R·C)` = 1 / (2π × 1600 × 100e-9) ≈ **995 Hz**
- No active parts, no power rails — simplest possible pipeline exercise.

**Validation criteria:**
- ERC: passes trivially (two-terminal passive network, no unconnected pins possible
  in this topology)
- ngspice AC analysis: sweep 1 Hz–100 kHz (decade sweep, 20 pts/decade), confirm
  −3dB point falls at 995 Hz ± 5% and rolloff is −20dB/decade above fc (first-order
  response)
- BOM: exactly 2 line items (R1, C1), both basic-library parts on JLCPCB — this
  circuit should never hit an extended-parts fee
- Layout: 2 footprints, no routing complexity — good test that DFM/panel stages
  don't choke on a near-trivial board

---

## Circuit 2: Non-Inverting Op-Amp Gain Stage

**Topology:** single op-amp, non-inverting configuration. Signal into +in. Output
fed back through R2 to −in; R1 from −in to GND sets the gain ratio.

**Components:**
| Ref | Part | Value | Notes |
|-----|------|-------|-------|
| U1  | Op-amp | TL072 (use channel A only) | Same part already in the slew-limiter buffer stages — reuse familiarity |
| R1  | Resistor | 1.0 kΩ | Gain-setting, −in to GND |
| R2  | Resistor | 10 kΩ | Feedback, output to −in |
| C1  | Capacitor | 100 nF | Local decoupling, V+ to GND |
| C2  | Capacitor | 100 nF | Local decoupling, V− to GND |

**Power:** dual ±12V rails (V+, V−, GND) — matches your existing Eurorack bus
convention even though this isn't a Eurorack module, so the same power-header
habits and decoupling-cap-at-the-pin discipline from Section on grounding
mistakes gets exercised here too.

**Target spec:**
- Gain: `Av = 1 + R2/R1` = 1 + (10k/1k) = **11** (20.8 dB)
- DC-coupled, no input/output blocking caps — deliberately simple, not meant to be
  audio-production-ready

**Validation criteria:**
- ERC: passes; also a first real test of "does ERC catch a missing power
  connection" — worth deliberately breaking the power net once in testing to
  confirm ERC actually flags it
- ngspice AC analysis: confirm flat gain of 11 (20.8 dB) across the op-amp's usable
  bandwidth, matches hand calc within a few %
- BOM: 5 line items (U1, R1, R2, C1, C2) — first circuit with a real IC in the BOM/
  CPL step and first real JLCPCB parts-catalog stock check
- Layout: first test of "decoupling cap physically close to the IC power pin" per
  the grounding/routing discipline we discussed for the phono pre — good low-
  stakes place to build that layout habit

---

## Open items before handing to SKiDL/Claude Code

- [ ] Confirm TL072 footprint choice (SOIC-8 vs DIP-8) — SOIC-8 assumed above to
      match JLCPCB SMT assembly flow; flag if DIP-8 hand-soldering is preferred
      for these throwaway test boards
- [ ] Confirm whether Circuit 2 should get eurorack-style jacks (for realism) or
      simple test points (simpler BOM, faster to lay out) — test points recommended
      for Phase 0 since these aren't meant to ship as products
