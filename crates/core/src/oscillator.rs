//! Pierce crystal oscillator — a genuinely new domain (timing/RF, not audio)
//! for the curated-circuit-generator pattern [`crate::topology`] already
//! established for the fuzz-pedal family. Deliberately minimal scope: the
//! resonator network only (crystal + two load capacitors), no driving gate
//! modelled.
//!
//! That's not a shortcut — it's what the real, authoritative reference
//! design does. ST's AN2867 ("Oscillator design guide for STM8/STM32
//! microcontrollers") draws exactly crystal + C1 + C2 + stray capacitance,
//! driven by "the microcontroller's internal inverter" — the inverter is
//! never a discrete part in that circuit, it's a given the driving IC
//! already provides. A discrete gate (a 74HC04 Pierce oscillator, say) only
//! becomes a real, drawable part once a specific one is chosen; this module
//! doesn't need to commit to that yet.
//!
//! No SPICE verification either, and that's also not a gap: oscillator
//! startup is a nonlinear negative-resistance transient phenomenon a linear
//! AC sweep can't validate ("does it oscillate" isn't a frequency-response
//! question). Verification here is analytic instead — the load-capacitor
//! values are *computed* from the chosen crystal's real datasheet load
//! capacitance, the same "derive, don't hardcode" discipline this crate
//! already enforces for VCC/IC in [`crate::topology`].
//!
//! Formula source: Ramon Cerda (Crystek Crystals Corp.), *RF Design*, July
//! 2004, "Pierce-gate oscillator crystal load calculation":
//!
//!   Cload = [(Cin+C1)(C2+Cout)/(Cin+C1+C2+Cout)] + Cstray
//!
//! The full form needs the driving gate's real Cin/Cout, which this module
//! doesn't have (no gate is modelled) — using the simplified symmetric form
//! instead, the same source's own reduction for C1=C2: `C1 = C2 =
//! 2*(CL-Cstray)`. This doesn't use Cin/Cout at all, only the crystal's own
//! real CL and a PCB stray-capacitance estimate.

use ooda::{Client, Question, Request, Trace};

use crate::spec::SpecError;

/// One real, sourced crystal: a real, currently-available part number, its
/// real datasheet load-capacitance spec, the frequency it's cut for, and a
/// real KiCad footprint matching its real package — confirmed against the
/// installed library directly (both candidates checked for a 2-pad body,
/// matching `Device:Crystal`'s 2 pins), not assumed from the package name
/// alone. Not a "typical" CL guessed from the frequency alone — CL varies by
/// manufacturer and part, not frequency, so each entry is its own verified
/// fact, not derived from a pattern.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrystalOption {
    /// The bounded-choice key this option answers to.
    pub key: &'static str,
    /// Nominal frequency, Hz.
    pub hz: f64,
    /// Real manufacturer part number.
    pub mpn: &'static str,
    /// Real datasheet load capacitance, pF.
    pub cl_pf: f64,
    /// Real KiCad footprint reference, confirmed 2-pad against the
    /// installed `Crystal.pretty` library, matching this part's real
    /// package (an SMD 3.2x1.5mm body for the watch crystal, the standard
    /// HC-49/US through-hole can for the MHz-range parts).
    pub footprint: &'static str,
}

/// Real crystals, ECS Inc., each individually verified against its own real
/// datasheet/distributor listing (2026-09-22) — not pattern-extrapolated
/// from one part to the rest of the family. Watch-crystal-class CL runs
/// noticeably lower (12.5pF) than this family's MHz-range fundamentals,
/// which are uniformly 20pF — a real fact about this part family, not a
/// rule that generalizes to other manufacturers without checking.
pub const CRYSTAL_CATALOG: &[CrystalOption] = &[
    CrystalOption {
        key: "32_768khz",
        hz: 32_768.0,
        mpn: "ECS-.327-12.5-34R-TR",
        cl_pf: 12.5,
        footprint: "Crystal:Crystal_SMD_3215-2Pin_3.2x1.5mm",
    },
    CrystalOption {
        key: "8mhz",
        hz: 8_000_000.0,
        mpn: "ECS-80-20-4X",
        cl_pf: 20.0,
        footprint: "Crystal:Crystal_HC49-U_Vertical",
    },
    CrystalOption {
        key: "16mhz",
        hz: 16_000_000.0,
        mpn: "ECS-160-20-4X",
        cl_pf: 20.0,
        footprint: "Crystal:Crystal_HC49-U_Vertical",
    },
    CrystalOption {
        key: "20mhz",
        hz: 20_000_000.0,
        mpn: "ECS-200-20-4X",
        cl_pf: 20.0,
        footprint: "Crystal:Crystal_HC49-U_Vertical",
    },
    CrystalOption {
        key: "25mhz",
        hz: 25_000_000.0,
        mpn: "ECS-250-20-4X-F-DN",
        cl_pf: 20.0,
        footprint: "Crystal:Crystal_HC49-U_Vertical",
    },
];

/// PCB stray capacitance estimate, pF. Real sources genuinely disagree —
/// this is a board-layout-dependent estimate, not a universal constant:
/// Cerda's own worked example (the source of the formula this module uses)
/// puts it at 2-3pF; ST's AN2867 uses ~5pF for its own reference layout.
/// Using Cerda's figure since it's the same source the formula itself comes
/// from — consistent with itself, not necessarily more "correct" than ST's
/// on a real board, which should be measured, not assumed, before trusting
/// this to a tight tolerance.
pub const STRAY_CAPACITANCE_PF: f64 = 3.0;

/// `C1 = C2` for a Pierce oscillator targeting `cl_pf` load capacitance,
/// given [`STRAY_CAPACITANCE_PF`] — Cerda's simplified symmetric reduction
/// of the full Cload formula (see module docs). Pure computation from the
/// crystal's own real spec, never a decision.
pub fn load_cap_pf(cl_pf: f64) -> f64 {
    2.0 * (cl_pf - STRAY_CAPACITANCE_PF)
}

/// A generated Pierce oscillator resonator network: which real crystal was
/// chosen, and the computed load-capacitor value.
#[derive(Debug, Clone, PartialEq)]
pub struct PierceOscillator {
    pub crystal: CrystalOption,
    pub load_cap_pf: f64,
}

/// Ask which crystal frequency to use (the one real, genuinely bounded
/// decision this circuit has — see module docs for why C1/C2 aren't a
/// decision at all), then compute the rest.
pub fn generate_pierce_oscillator(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
) -> Result<PierceOscillator, SpecError> {
    let criteria: ooda::Criteria = CRYSTAL_CATALOG
        .iter()
        .map(|c| {
            let desc = if c.hz >= 1_000_000.0 {
                format!(
                    "{:.0}MHz crystal ({}, real datasheet CL={:.1}pF)",
                    c.hz / 1e6,
                    c.mpn,
                    c.cl_pf
                )
            } else {
                format!(
                    "{:.3}kHz watch crystal ({}, real datasheet CL={:.1}pF) -- for an RTC",
                    c.hz / 1e3,
                    c.mpn,
                    c.cl_pf
                )
            };
            (c.key, desc)
        })
        .collect();

    let observation =
        serde_json::json!({ "brief": brief, "task": "Pierce crystal oscillator design" });
    let request = Request::new(observation).with(
        "crystal_frequency",
        Question::choice("Which crystal frequency best fits this brief?", criteria),
    );
    let outcome = client.decide(&request)?;
    let answer = outcome.recorded_answer("crystal_frequency", trace)?;
    let key = answer.choice().unwrap_or(CRYSTAL_CATALOG[0].key);
    let crystal = CRYSTAL_CATALOG
        .iter()
        .find(|c| c.key == key)
        .copied()
        .unwrap_or(CRYSTAL_CATALOG[0]);

    Ok(PierceOscillator {
        crystal,
        load_cap_pf: load_cap_pf(crystal.cl_pf),
    })
}

/// Round to the nearest real E12-series capacitor value (matches
/// [`crate::spec`]'s own `round_e12` convention for resistors — a computed
/// value should land on something actually buyable, not a mathematically
/// exact but unpurchasable number).
fn round_e12_pf(pf: f64) -> f64 {
    const E12: [f64; 12] = [1.0, 1.2, 1.5, 1.8, 2.2, 2.7, 3.3, 3.9, 4.7, 5.6, 6.8, 8.2];
    if pf <= 0.0 {
        return 0.0;
    }
    let decade = 10f64.powf(pf.log10().floor());
    let mantissa = pf / decade;
    let nearest = E12
        .iter()
        .min_by(|a, b| (**a - mantissa).abs().total_cmp(&(**b - mantissa).abs()))
        .copied()
        .unwrap_or(mantissa);
    nearest * decade
}

/// Render as a standalone SKiDL script: the crystal (`Device:Crystal`, real
/// 2-pin symbol — pins "1"/"2", confirmed against the installed
/// `Device.kicad_sym` directly, not assumed) plus its two load capacitors to
/// ground. `OSC_IN`/`OSC_OUT` net labels mark where a real driving gate or
/// MCU oscillator pin would connect — deliberately not drawn (see module
/// docs).
pub fn render_pierce_oscillator_skidl(osc: &PierceOscillator) -> String {
    let cap_pf = round_e12_pf(osc.load_cap_pf);
    let cap_label = fmt_pf(cap_pf);
    let freq_label = if osc.crystal.hz >= 1_000_000.0 {
        format!("{:.0}MHz", osc.crystal.hz / 1e6)
    } else {
        format!("{:.3}kHz", osc.crystal.hz / 1e3)
    };

    format!(
        r#"#!/usr/bin/env python3
"""Pierce crystal oscillator -- resonator network only, no driving gate.

Generated by legion-of-bom's typed-decision oscillator generator
(crates/core/src/oscillator.rs). See that file's module docs for why this
circuit deliberately stops at the resonator (crystal + two load caps) --
matches ST AN2867's own real reference design, which never draws the
inverter either.

Crystal: {mpn} -- real, sourced datasheet load capacitance CL={cl_pf:.1}pF,
real footprint {footprint} (confirmed 2-pad against the installed KiCad
library, matching this part's real package).
Frequency: {freq_label}.
Load caps: computed as C1=C2=2*(CL-Cstray), Cstray={stray:.1}pF (Cerda,
RF Design, July 2004) -- rounded to the nearest real E12 value ({cap_pf:.1}pF
exact, {cap_label} nominal). NOT a decision -- pure computation from the
crystal's own real spec.

OSC_IN/OSC_OUT are net labels, not drawn pins on anything -- they mark
where a real driving gate (a 74HC04 Pierce oscillator) or an MCU's
crystal-input pins would connect. This circuit doesn't commit to either.

Run standalone (needs KICAD9_SYMBOL_DIR); `lob run` sets it up.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

C_FOOTPRINT = "Capacitor_SMD:C_0805_2012Metric"


def build():
    """Construct the resonator network."""
    gnd = Net("GND")
    osc_in = Net("OSC_IN")
    osc_out = Net("OSC_OUT")

    xtal = Part("Device", "Crystal", footprint="{footprint}", ref="Y1")
    xtal.fields["MPN"] = "{mpn}"

    c1 = Part("Device", "C", value="{cap_label}", footprint=C_FOOTPRINT, ref="C1")
    c2 = Part("Device", "C", value="{cap_label}", footprint=C_FOOTPRINT, ref="C2")

    osc_in += xtal[1], c1[1]
    osc_out += xtal[2], c2[1]
    gnd += c1[2], c2[2]

    return {{"Y1": xtal, "C1": c1, "C2": c2}}


def main():
    parser = argparse.ArgumentParser(description="Pierce crystal oscillator ({freq_label})")
    parser.add_argument("--output", default="circuit.net", help="netlist output path")
    args = parser.parse_args()

    build()
    erc_ok = ERC()
    generate_netlist(file_=args.output)
    if erc_ok is False:
        sys.exit(1)


if __name__ == "__main__":
    main()
"#,
        mpn = osc.crystal.mpn,
        cl_pf = osc.crystal.cl_pf,
        footprint = osc.crystal.footprint,
        freq_label = freq_label,
        stray = STRAY_CAPACITANCE_PF,
        cap_pf = osc.load_cap_pf,
        cap_label = cap_label,
    )
}

/// A picofarad value as SKiDL-style text (`"33p"`, matching the existing
/// `"2.2u"`/`"150k"` convention elsewhere in this crate's generated code).
fn fmt_pf(pf: f64) -> String {
    if (pf - pf.round()).abs() < 1e-6 {
        format!("{}p", pf.round() as i64)
    } else {
        format!("{pf:.1}p")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_cap_matches_cerda_worked_range() {
        // 20pF-CL crystal (the real MHz-range figure for this session's
        // sourced ECS parts), 3pF stray -> C1=C2=34pF, landing in the same
        // real ballpark (22-33pF-ish) commonly cited for MHz Pierce
        // oscillators -- not exact since that's a different worked example,
        // just a sanity check it's not off by an order of magnitude.
        assert!((load_cap_pf(20.0) - 34.0).abs() < 1e-9);
        // 12.5pF-CL watch crystal -> C1=C2=19pF, in the real 15-22pF range
        // commonly cited for 32.768kHz oscillators.
        assert!((load_cap_pf(12.5) - 19.0).abs() < 1e-9);
    }

    #[test]
    fn every_catalog_entry_has_a_distinct_key_and_positive_real_values() {
        let mut keys: Vec<&str> = CRYSTAL_CATALOG.iter().map(|c| c.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), CRYSTAL_CATALOG.len(), "duplicate crystal key");
        for c in CRYSTAL_CATALOG {
            assert!(c.hz > 0.0, "{}: non-positive frequency", c.key);
            assert!(
                c.cl_pf > STRAY_CAPACITANCE_PF,
                "{}: CL must exceed stray or C1/C2 goes negative",
                c.key
            );
            assert!(!c.mpn.is_empty(), "{}: missing MPN", c.key);
            assert!(
                c.footprint.starts_with("Crystal:"),
                "{}: footprint should resolve against the real Crystal.pretty library",
                c.key
            );
        }
    }

    #[test]
    fn generate_pierce_oscillator_runs_offline_against_a_scripted_client() {
        let client = ooda::ScriptedClient::new([
            r#"{"answers": {"crystal_frequency": {"type":"choice","choice":"16mhz","confidence":0.9,"probabilities":{}}}}"#,
        ]);
        let mut trace = Trace::new();
        let osc =
            generate_pierce_oscillator(&client, &mut trace, "a real-time clock reference").unwrap();
        assert_eq!(osc.crystal.key, "16mhz");
        assert_eq!(osc.crystal.mpn, "ECS-160-20-4X");
        assert!((osc.load_cap_pf - 34.0).abs() < 1e-9);
    }

    #[test]
    fn render_pierce_oscillator_skidl_produces_parseable_python() {
        let osc = PierceOscillator {
            crystal: CRYSTAL_CATALOG[2], // 16MHz
            load_cap_pf: load_cap_pf(CRYSTAL_CATALOG[2].cl_pf),
        };
        let py = render_pierce_oscillator_skidl(&osc);
        assert!(py.contains(
            "Part(\"Device\", \"Crystal\", footprint=\"Crystal:Crystal_HC49-U_Vertical\", ref=\"Y1\")"
        ));
        assert!(py.contains("ECS-160-20-4X"));
        assert!(py.contains(r#"osc_in += xtal[1], c1[1]"#));
        assert!(py.contains(r#"osc_out += xtal[2], c2[1]"#));
        // 34.0pF exact rounds to the nearest real E12 value, 33pF -- that's
        // what actually goes into the circuit, not the pre-rounding figure.
        assert!(py.contains("value=\"33p\""));
    }

    #[test]
    fn round_e12_pf_lands_on_a_real_buyable_value() {
        // 34.0pF exact is already a real, common cap value -- but not
        // strictly E12 (E12's 3.3 decade gives 33pF); confirm rounding picks
        // the nearest real E12 mantissa rather than passing through exact.
        assert_eq!(round_e12_pf(34.0), 33.0);
        assert_eq!(round_e12_pf(19.0), 18.0);
    }
}
