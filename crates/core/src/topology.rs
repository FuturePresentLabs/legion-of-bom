//! Fuzz-pedal topology as a DAG: a chain of curated gain-stage nodes,
//! assembled via sequential typed decisions -- "rail -> node -> node -> ...
//! -> output" -- rather than picked from one fixed two-stage template.
//!
//! Every stage is still a real, vetted common-emitter BJT block (the same
//! bias math [`crate::spec`] already uses) -- per DESIGN.md's "curated, not
//! generated" stance, what's agentic here is *composition* (how many
//! stages, what each one's voice/character is, how much current budget the
//! bias points spend), never invention of a new circuit primitive.
//!
//! # Constraints vs. decisions
//!
//! Supply voltage, enclosure size, mono signal path, and true-bypass
//! switching are not decisions -- they're structural requirements a real
//! design request specifies up front (the way "9V, 125B enclosure, true
//! bypass" reads as a client's brief, not an open question). Those live in
//! [`FuzzConstraints`], a plain parameter the caller supplies. What stays a
//! genuine decision is the circuit's own composition: how many gain stages,
//! each one's voicing/character, whether to spend more quiescent current on
//! a stiffer bias point, whether to include a tone stack.
//!
//! # The bypass/jack/rail scaffold
//!
//! The true-bypass 3PDT footswitch, the input/output jacks, and the LED
//! indicator are a fixed, always-present structure -- there's no design
//! space here to ask about, every mono true-bypass pedal wires this the
//! same way. [`render_chain_skidl`] renders it around whatever chain the
//! DAG decided on, using real KiCad symbols confirmed against the installed
//! library (`Connector_Audio:AudioJack2`, `Switch:SW_SPDT` x3 -- no stock
//! 3PDT symbol exists, so each pole is its own SPDT part, per normal KiCad
//! practice for multi-gang switches -- and `Device:LED`), not assumed pin
//! names.

use serde::{Deserialize, Serialize};

use ooda::{Client, Criteria, Question, Request, Trace};

use crate::spec::{
    bias_network, expect_choice, expect_noul, expect_score, fmt_ohms, round_e12, BiasVoice,
    EnclosureSize, GainCharacter, SpecError, BETA, FUZZ_POT_MAX_OHMS, STAGE2_EMITTER_OHMS,
};

/// Fixed constraints on a fuzz-pedal design request -- not typed decisions.
/// A real design brief specifies these the way a client specifies form
/// factor and power budget to an engineer: as requirements, not creative
/// choices. Contrast [`GainStage`]'s `bias_voice`/`gain_character`, which
/// stay genuine per-stage decisions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FuzzConstraints {
    /// Supply rail voltage (a 9V battery/adapter is the near-universal pedal
    /// convention; some designs run 18V via two batteries or a charge-pump
    /// boost for extra headroom).
    pub vcc: f64,
    pub enclosure_size: EnclosureSize,
}

/// A stage's quiescent-current target is a real trade-off, not an assumed
/// number: lower Ic means longer 9V-battery life and less loading on the
/// previous stage, at the cost of a bias point that's less stiff against
/// hFE spread and a higher-impedance (more rolloff-prone) collector node;
/// higher Ic is the opposite trade. `headroom` in `0.0..=1.0` (0 = most
/// battery-conscious, 1 = stiffest bias) interpolates between a genuinely
/// low-power floor and a genuinely stiff-bias ceiling for a small-signal
/// BJT audio stage -- the collector resistor this implies then falls out of
/// [`bias_network`]'s own formula, not a second assumption.
fn target_ic_ma(headroom: f64) -> f64 {
    const BATTERY_FRIENDLY_MA: f64 = 0.15;
    const STIFF_BIAS_MA: f64 = 1.0;
    BATTERY_FRIENDLY_MA + headroom.clamp(0.0, 1.0) * (STIFF_BIAS_MA - BATTERY_FRIENDLY_MA)
}

const HEADROOM_LEVELS: &[&str] = &["battery_friendly", "balanced", "stiff_bias"];

/// One gain stage in the chain -- a real, vetted common-emitter BJT block.
/// The first stage gets the Fuzz-pot emitter treatment (the classic
/// Fuzz-Face-family mechanism); every later stage gets a fixed emitter
/// resistor, same convention [`crate::spec`]'s fixed two-stage circuit uses
/// for its second stage.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GainStage {
    pub bias_voice: BiasVoice,
    pub gain_character: GainCharacter,
}

/// A fuzz-pedal circuit assembled as a chain: IN -> \[GainStage\]+ -> (tone
/// stack)? -> volume -> OUT, wrapped in the fixed true-bypass/jack scaffold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuzzChain {
    pub vcc: f64,
    pub ic_ma: f64,
    pub enclosure_size: EnclosureSize,
    pub stages: Vec<GainStage>,
    pub tone_stack: bool,
}

const MAX_STAGES: usize = 4;

/// Assemble a fuzz chain via sequential decisions -- a real DAG walk, not
/// one batched call, because whether stage N+1 exists depends on what was
/// decided about stage N. Every resolved answer is appended to `trace`.
pub fn generate_fuzz_chain(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
    constraints: FuzzConstraints,
) -> Result<FuzzChain, SpecError> {
    // Current headroom and tone-stack presence are independent of stage
    // count and of each other -- one batched call, same as the fixed
    // two-stage generator's own independent-question batch.
    let request = Request::new(brief)
        .with(
            "current_headroom",
            Question::score(
                "Rate how much this design should favor battery life (low quiescent \
                 current) versus a stiffer, lower-impedance bias point (higher current).",
                HEADROOM_LEVELS
                    .iter()
                    .map(|l| (*l, *l))
                    .collect::<Criteria>(),
            ),
        )
        .with(
            "tone_stack",
            Question::noul_with_context(
                "Should this design include a simple fixed treble-cut tone network ahead \
                 of the volume control?",
                "yes, include a simple tone network",
                "no, direct output to volume",
            ),
        );
    let outcome = client.decide(&request)?;
    let headroom_score = expect_score(&outcome, trace, "current_headroom")?;
    let headroom = headroom_score.clamp(0.0, (HEADROOM_LEVELS.len() - 1) as f64)
        / (HEADROOM_LEVELS.len() - 1) as f64;
    let tone_stack = expect_noul(&outcome, trace, "tone_stack")? >= 0.5;
    let ic_ma = target_ic_ma(headroom);

    let mut stages = vec![decide_stage(client, trace, brief, 0)?];
    while stages.len() < MAX_STAGES {
        let idx = stages.len();
        let key = format!("stage{}_continue", idx + 1);
        let request = Request::new(brief).with(
            &key,
            Question::noul_with_context(
                format!(
                    "This fuzz pedal has {} gain stage(s) so far. Should another \
                     common-emitter gain stage be cascaded on, or is the signal chain \
                     complete?",
                    stages.len()
                ),
                "yes, add another gain stage",
                "no, the chain is complete",
            ),
        );
        let outcome = client.decide(&request)?;
        let go_on = expect_noul(&outcome, trace, &key)? >= 0.5;
        if !go_on {
            break;
        }
        stages.push(decide_stage(client, trace, brief, idx)?);
    }

    Ok(FuzzChain {
        vcc: constraints.vcc,
        ic_ma,
        enclosure_size: constraints.enclosure_size,
        stages,
        tone_stack,
    })
}

fn decide_stage(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
    idx: usize,
) -> Result<GainStage, SpecError> {
    let bias_key = format!("stage{}_bias_voice", idx + 1);
    let gain_key = format!("stage{}_gain_character", idx + 1);
    let request = Request::new(brief)
        .with(
            &bias_key,
            Question::choice(
                format!("Pick stage {}'s collector bias voicing.", idx + 1),
                BiasVoice::CHOICES.iter().copied().collect::<Criteria>(),
            ),
        )
        .with(
            &gain_key,
            Question::score(
                format!("Rate stage {}'s available gain/instability.", idx + 1),
                GainCharacter::LEVELS
                    .iter()
                    .map(|l| (*l, *l))
                    .collect::<Criteria>(),
            ),
        );
    let outcome = client.decide(&request)?;
    let bias_voice = BiasVoice::from_key(&expect_choice(&outcome, trace, &bias_key)?);
    let gain_character = GainCharacter::from_score(expect_score(&outcome, trace, &gain_key)?);
    Ok(GainStage {
        bias_voice,
        gain_character,
    })
}

/// Render a [`FuzzChain`] as a standalone SKiDL script: the fixed
/// jack/bypass/LED scaffold wrapping the DAG-assembled gain-stage chain. A
/// pure function of the chain -- no decision calls, no network -- same
/// contract [`crate::spec::render_skidl`] holds for the fixed two-stage
/// circuit.
pub fn render_chain_skidl(chain: &FuzzChain) -> String {
    let mut r_n = 0u32;
    let mut c_n = 0u32;
    let mut rv_n = 0u32;
    let mut q_n = 0u32;
    let next_ref = |prefix: &str, n: &mut u32| {
        *n += 1;
        format!("{prefix}{n}")
    };

    let mut build = String::new();
    let mut bom: Vec<(String, String)> = Vec::new();

    // -- Gain-stage chain, one node at a time. --
    let mut prior_out: Option<String> = None;
    for (i, stage) in chain.stages.iter().enumerate() {
        let q_ref = next_ref("Q", &mut q_n);
        let q_var = q_ref.to_lowercase();
        build.push_str(&format!("    {q_var} = _npn(\"{q_ref}\")\n"));

        let emitter_ohms = if i == 0 {
            stage.gain_character.floor_ohms() + FUZZ_POT_MAX_OHMS / 2.0
        } else {
            STAGE2_EMITTER_OHMS
        };
        let (rc, r_top, r_bottom) = bias_network(
            chain.vcc,
            stage.bias_voice.vc_fraction(),
            chain.ic_ma,
            emitter_ohms,
            BETA,
        );

        let r_top_ref = next_ref("R", &mut r_n);
        let r_top_var = r_top_ref.to_lowercase();
        build.push_str(&format!(
            "    {r_top_var} = Part(\"Device\", \"R\", value=\"{}\", footprint=R_FOOTPRINT, ref=\"{r_top_ref}\")\n",
            fmt_ohms(r_top)
        ));
        let r_bottom_ref = next_ref("R", &mut r_n);
        let r_bottom_var = r_bottom_ref.to_lowercase();
        build.push_str(&format!(
            "    {r_bottom_var} = Part(\"Device\", \"R\", value=\"{}\", footprint=R_FOOTPRINT, ref=\"{r_bottom_ref}\")\n",
            fmt_ohms(r_bottom)
        ));
        let r_c_ref = next_ref("R", &mut r_n);
        let r_c_var = r_c_ref.to_lowercase();
        build.push_str(&format!(
            "    {r_c_var} = Part(\"Device\", \"R\", value=\"{}\", footprint=R_FOOTPRINT, ref=\"{r_c_ref}\")\n",
            fmt_ohms(rc)
        ));

        let base_var = format!("base{}", i + 1);
        let base_net = format!("Q{}_BASE", i + 1);
        let c_ref = next_ref("C", &mut c_n);
        let c_var = c_ref.to_lowercase();
        match &prior_out {
            None => {
                build.push_str(&format!(
                    "    {c_var} = Part(\"Device\", \"C\", value=\"2.2u\", footprint=C_FOOTPRINT, ref=\"{c_ref}\")\n"
                ));
                build.push_str(&format!("    vin += {c_var}[1]\n"));
            }
            Some(prior) => {
                build.push_str(&format!(
                    "    {c_var} = Part(\"Device\", \"C\", value=\"100n\", footprint=C_FOOTPRINT, ref=\"{c_ref}\")\n"
                ));
                build.push_str(&format!("    {prior} += {c_var}[1]\n"));
            }
        }
        build.push_str(&format!("    {base_var} = Net(\"{base_net}\")\n"));
        build.push_str(&format!(
            "    {base_var} += {c_var}[2], {r_top_var}[2], {r_bottom_var}[1], {q_var}[\"B\"]\n"
        ));
        build.push_str(&format!("    vcc += {r_top_var}[1]\n"));
        build.push_str(&format!("    gnd += {r_bottom_var}[2]\n"));

        if i == 0 {
            let floor_ref = next_ref("R", &mut r_n);
            let floor_var = floor_ref.to_lowercase();
            build.push_str(&format!(
                "    {floor_var} = Part(\"Device\", \"R\", value=\"{}\", footprint=R_FOOTPRINT, ref=\"{floor_ref}\")\n",
                fmt_ohms(round_e12(stage.gain_character.floor_ohms()))
            ));
            let pot_ref = next_ref("RV", &mut rv_n);
            let pot_var = pot_ref.to_lowercase();
            build.push_str(&format!(
                "    {pot_var} = Part(\"Device\", \"R_Potentiometer\", value=\"{}\", footprint=\"Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical\", ref=\"{pot_ref}\")\n",
                fmt_ohms(FUZZ_POT_MAX_OHMS)
            ));
            build.push_str("    fuzz_a = Net(\"FUZZ_A\")\n");
            build.push_str(&format!("    {q_var}[\"E\"] += fuzz_a\n"));
            build.push_str(&format!("    fuzz_a += {floor_var}[1]\n"));
            build.push_str("    fuzz_b = Net(\"FUZZ_B\")\n");
            build.push_str(&format!("    {floor_var}[2] += fuzz_b\n"));
            build.push_str(&format!("    fuzz_b += {pot_var}[1]\n"));
            build.push_str(&format!("    gnd += {pot_var}[2], {pot_var}[3]\n"));
            bom.push((pot_ref, pot_var));
            bom.push((floor_ref, floor_var));
        } else {
            let e_ref = next_ref("R", &mut r_n);
            let e_var = e_ref.to_lowercase();
            build.push_str(&format!(
                "    {e_var} = Part(\"Device\", \"R\", value=\"{}\", footprint=R_FOOTPRINT, ref=\"{e_ref}\")\n",
                fmt_ohms(STAGE2_EMITTER_OHMS)
            ));
            let e_net = format!("e{}", i + 1);
            build.push_str(&format!("    {e_net} = Net(\"Q{}_EMITTER\")\n", i + 1));
            build.push_str(&format!("    {e_net} += {q_var}[\"E\"], {e_var}[1]\n"));
            build.push_str(&format!("    gnd += {e_var}[2]\n"));
            bom.push((e_ref, e_var));
        }

        let out_var = format!("stage{}_out", i + 1);
        build.push_str(&format!("    {out_var} = Net(\"STAGE{}_OUT\")\n", i + 1));
        build.push_str(&format!("    {out_var} += {q_var}[\"C\"], {r_c_var}[1]\n"));
        build.push_str(&format!("    vcc += {r_c_var}[2]\n"));

        bom.push((q_ref, q_var));
        bom.push((r_top_ref, r_top_var));
        bom.push((r_bottom_ref, r_bottom_var));
        bom.push((r_c_ref, r_c_var));
        bom.push((c_ref, c_var));

        prior_out = Some(out_var);
    }
    let last_out = prior_out.expect("at least one stage is always generated");

    // -- Output coupling + optional tone + volume. --
    let c_out_ref = next_ref("C", &mut c_n);
    let c_out_var = c_out_ref.to_lowercase();
    build.push_str(&format!(
        "    {c_out_var} = Part(\"Device\", \"C\", value=\"1u\", footprint=C_FOOTPRINT, ref=\"{c_out_ref}\")\n"
    ));
    build.push_str(&format!("    {last_out} += {c_out_var}[1]\n"));
    bom.push((c_out_ref, c_out_var.clone()));

    let vol_in = if chain.tone_stack {
        let r_tone_ref = next_ref("R", &mut r_n);
        let r_tone_var = r_tone_ref.to_lowercase();
        let c_tone_ref = next_ref("C", &mut c_n);
        let c_tone_var = c_tone_ref.to_lowercase();
        build.push_str(&format!(
            "    {r_tone_var} = Part(\"Device\", \"R\", value=\"4.7k\", footprint=R_FOOTPRINT, ref=\"{r_tone_ref}\")\n"
        ));
        build.push_str(&format!(
            "    {c_tone_var} = Part(\"Device\", \"C\", value=\"10n\", footprint=C_FOOTPRINT, ref=\"{c_tone_ref}\")\n"
        ));
        build.push_str("    tone_node = Net(\"TONE\")\n");
        build.push_str("    tone_mid = Net(\"TONE_MID\")\n");
        build.push_str(&format!(
            "    tone_node += {c_out_var}[2], {r_tone_var}[1]\n"
        ));
        build.push_str(&format!(
            "    tone_mid += {r_tone_var}[2], {c_tone_var}[1]\n"
        ));
        build.push_str(&format!("    gnd += {c_tone_var}[2]\n"));
        bom.push((r_tone_ref, r_tone_var));
        bom.push((c_tone_ref, c_tone_var));
        "tone_node".to_string()
    } else {
        format!("{c_out_var}[2]")
    };

    let pot_ref = next_ref("RV", &mut rv_n);
    let pot_var = pot_ref.to_lowercase();
    build.push_str(&format!(
        "    {pot_var} = Part(\"Device\", \"R_Potentiometer\", value=\"100k\", footprint=\"Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical\", ref=\"{pot_ref}\")\n"
    ));
    build.push_str(&format!("    {pot_var}[1] += {vol_in}\n"));
    build.push_str(&format!("    vout += {pot_var}[2]\n"));
    build.push_str(&format!("    gnd += {pot_var}[3]\n"));
    bom.push((pot_ref, pot_var));

    let bom_dict: String = bom
        .iter()
        .map(|(ref_, var)| format!("\"{ref_}\": {var}"))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#""""Fuzz pedal -- DAG-assembled from {n_stages} gain stage(s) via sequential
System One / Jev-compatible decisions (lob spec fuzz-chain), not picked from
one fixed template. Each stage is the same common-emitter, RC-coupled,
transistor-rail-clipping block the fixed two-stage generator uses; what's
decided here is how many are cascaded and each one's voicing/character, plus
the whole circuit's current-budget trade-off (battery life vs bias
stiffness) -- see the sibling .trace.json for the full decision record.

Constraints on this run (not decisions -- specified, not chosen):
  vcc             = {vcc}V
  enclosure_size  = {enclosure}

Decisions baked into this run:
  stages          = {n_stages}
  ic_ma           = {ic_ma:.3} (per-stage quiescent current, from the current-headroom decision)
  tone_stack      = {tone_stack}

True-bypass wiring: a 3PDT footswitch modelled as three Switch:SW_SPDT poles
(no stock 3PDT symbol exists in the installed KiCad library -- confirmed by
grepping it directly, not assumed -- so each pole is its own part, standard
KiCad practice for multi-gang switches with no combined symbol). Pole B is
each SW_SPDT's common pin; A is the engaged throw, C is the bypass throw
(SW1/SW2 share a BYPASS_LINK net on their C throws for direct pass-through;
SW3 switches the LED). Mono in/out via Connector_Audio:AudioJack2 (T=tip,
S=sleeve).

Q1.. use KiCad's generic Device:Q_NPN symbol, whose pins are letter-named
(B/C/E, not numbered) -- read directly from the installed Device.kicad_sym,
not recalled from memory. **What's still UNVERIFIED is the MMBT3904
footprint's physical lead order** -- a first-pass guess, not a verified
fact. `lob parts gate` will (correctly) block real board/BOM generation on
this MPN until a human confirms it against the datasheet, same as any other
unverified part (DESIGN.md 3.5/4.3).

SW1/SW2/SW3 carry no footprint yet -- they're one physical 3PDT part
represented as three schematic symbols (see above), and how that maps to a
single PCB footprint placement (vs. three) is a real board-layout question
this run doesn't answer. `lob board` isn't expected to place this circuit
correctly until that's resolved; `lob run`'s ERC/SPICE checks above don't
need a footprint at all, so they're unaffected.

Panel layout is NOT modelled here -- this file is the audio signal path plus
its true-bypass switching, checked by lob run (ERC + ngspice); panel/
mechanical layout is a separate stage.

Run standalone (needs KICAD9_SYMBOL_DIR); `lob run`/`lob board` set it up.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

R_FOOTPRINT = "Resistor_SMD:R_0805_2012Metric"
C_FOOTPRINT = "Capacitor_SMD:C_0805_2012Metric"
Q_FOOTPRINT = "Package_TO_SOT_SMD:SOT-23"
Q_MPN = "MMBT3904"  # NPN silicon, SOT-23 -- footprint lead order unverified, see docstring


def _npn(ref):
    """A generic NPN silicon transistor carrying its own behavioural SPICE model."""
    q = Part("Device", "Q_NPN", value="MMBT3904", footprint=Q_FOOTPRINT, ref=ref)
    q.fields["Sim.Device"] = "SUBCKT"
    q.fields["Sim.Name"] = "NPN_GENERIC"
    q.fields["Sim.Library"] = "lob_builtin.lib"
    q.fields["Sim.Pins"] = "B=b E=e C=c"  # Q_NPN's pin "numbers" are letters
    q.fields["MPN"] = Q_MPN
    return q


def build():
    """Construct the fuzz circuit -- true-bypass scaffold, then the DAG chain."""
    gnd = Net("GND")
    vcc = Net("+9V")

    # -- I/O jacks (mono TS: T=tip/signal, S=sleeve/ground). --
    j_in = Part("Connector_Audio", "AudioJack2", footprint="Jack_6.35mm_TS", ref="J1")
    j_out = Part("Connector_Audio", "AudioJack2", footprint="Jack_6.35mm_TS", ref="J2")
    gnd += j_in["S"], j_out["S"]

    # -- True-bypass 3PDT footswitch: three SPDT poles (B=common, A=engaged,
    # C=bypass). SW1/SW2 share BYPASS_LINK on their bypass throws for a
    # direct pass-through; each engaged throw feeds/returns from the DAG
    # chain below via vin/vout.
    #
    # A mechanical switch's contacts simulate as open regardless of throw
    # (crate::spice: "the netlist records its wiring, never which way the
    # lever is thrown" -- true of every switch in this codebase, not special
    # to this circuit), so "IN"/"OUT" -- the nets the sim harness actually
    # injects/measures at -- are the DAG chain's own real boundary, not the
    # jack side of the switch: only that side has a path SPICE will simulate
    # as connected. The jack-facing nets get their own name since they carry
    # no simulation meaning (they're gated by a switch position the deck
    # never models either way), only real hardware/board-layout meaning.
    sw_in = Part("Switch", "SW_SPDT", ref="SW1")
    sw_out = Part("Switch", "SW_SPDT", ref="SW2")
    sw_led = Part("Switch", "SW_SPDT", ref="SW3")

    bypass_link = Net("BYPASS_LINK")
    jack_in = Net("JACK_IN")
    jack_out = Net("JACK_OUT")
    vin = Net("IN")
    vout = Net("OUT")

    jack_in += j_in["T"], sw_in["B"]
    sw_in["C"] += bypass_link
    sw_in["A"] += vin

    jack_out += j_out["T"], sw_out["B"]
    sw_out["C"] += bypass_link
    sw_out["A"] += vout

    # -- Bypass-indicator LED: lit only in the engaged position. Generic
    # diode SPICE mapping (crate::spice needs one for any non-R/C/L part),
    # not a specific real LED's datasheet -- see lob_builtin.lib.
    led = Part("Device", "LED", footprint="LED_5mm", ref="D1")
    led.fields["Sim.Device"] = "SUBCKT"
    led.fields["Sim.Name"] = "LED_GENERIC"
    led.fields["Sim.Library"] = "lob_builtin.lib"
    led.fields["Sim.Pins"] = "A=a K=k"
    r_led = Part("Device", "R", value="4.7k", footprint=R_FOOTPRINT, ref="R_LED")
    gnd += led["K"]
    led["A"] += r_led[2]
    r_led[1] += sw_led["A"]
    vcc += sw_led["B"]
    gnd += sw_led["C"]  # unused bypass-position throw tied to ground, not left floating

{build}
    return {{
        "J1": j_in, "J2": j_out,
        "SW1": sw_in, "SW2": sw_out, "SW3": sw_led,
        "D1": led, "R_LED": r_led,
        {bom_dict},
    }}


def main(argv=None):
    parser = argparse.ArgumentParser(description="Emit the fuzz-pedal KiCad netlist.")
    parser.add_argument("--output", "-o", default="fuzz_pedal.net")
    args = parser.parse_args(argv)

    build()
    ERC()
    generate_netlist(file_=args.output)
    print(f"wrote netlist: {{args.output}}", file=sys.stderr)


if __name__ == "__main__":
    main()
"#,
        n_stages = chain.stages.len(),
        vcc = chain.vcc,
        enclosure = chain.enclosure_size.key(),
        ic_ma = chain.ic_ma,
        tone_stack = chain.tone_stack,
        build = build,
        bom_dict = bom_dict,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> ooda::ScriptedClient {
        // One "continue? yes" then "continue? no" -> exactly 2 stages,
        // exercising both the fuzz-pot (stage 1) and fixed-emitter (stage
        // 2+) rendering paths in one offline test.
        ooda::ScriptedClient::new([
            r#"{"answers": {
                "current_headroom": {"type":"score","score":1.0,"confidence":0.7},
                "tone_stack": {"type":"boolean","probability":0.9}
            }}"#
            .to_string(),
            r#"{"answers": {
                "stage1_bias_voice": {"type":"choice","choice":"symmetric","confidence":0.8,"probabilities":{"symmetric":0.8}},
                "stage1_gain_character": {"type":"score","score":2.0,"confidence":0.75}
            }}"#
            .to_string(),
            r#"{"answers": {"stage2_continue": {"type":"boolean","probability":0.9}}}"#.to_string(),
            r#"{"answers": {
                "stage2_bias_voice": {"type":"choice","choice":"dark_asymmetric","confidence":0.6,"probabilities":{"dark_asymmetric":0.6}},
                "stage2_gain_character": {"type":"score","score":1.0,"confidence":0.7}
            }}"#
            .to_string(),
            r#"{"answers": {"stage3_continue": {"type":"boolean","probability":0.1}}}"#.to_string(),
        ])
    }

    #[test]
    fn generate_fuzz_chain_runs_offline_against_a_scripted_client() {
        let client = client();
        let mut trace = Trace::new();
        let constraints = FuzzConstraints {
            vcc: 9.0,
            enclosure_size: EnclosureSize::Size125B,
        };
        let chain =
            generate_fuzz_chain(&client, &mut trace, "vintage silicon fuzz", constraints).unwrap();
        assert_eq!(chain.stages.len(), 2);
        assert_eq!(chain.stages[0].bias_voice, BiasVoice::Symmetric);
        assert_eq!(chain.stages[1].bias_voice, BiasVoice::DarkAsymmetric);
        assert!(chain.tone_stack);
        assert_eq!(chain.vcc, 9.0);
        assert_eq!(chain.enclosure_size, EnclosureSize::Size125B);
        assert!(chain.ic_ma > 0.0);
        // 2 continue-checks (stage2, stage3) + 2 per-stage batches + 1 headroom/tone batch.
        assert_eq!(trace.records().len(), 2 + 2 * 2 + 2);
    }

    #[test]
    fn target_ic_interpolates_between_the_documented_bounds() {
        assert!((target_ic_ma(0.0) - 0.15).abs() < 1e-9);
        assert!((target_ic_ma(1.0) - 1.0).abs() < 1e-9);
        let mid = target_ic_ma(0.5);
        assert!(mid > 0.15 && mid < 1.0);
    }

    #[test]
    fn render_chain_skidl_produces_parseable_python_for_a_single_stage() {
        let chain = FuzzChain {
            vcc: 9.0,
            ic_ma: 0.5,
            enclosure_size: EnclosureSize::Size1590B,
            stages: vec![GainStage {
                bias_voice: BiasVoice::Symmetric,
                gain_character: GainCharacter::Balanced,
            }],
            tone_stack: false,
        };
        let py = render_chain_skidl(&chain);
        assert!(py.contains("def build():"));
        assert!(py.contains("AudioJack2"));
        assert!(py.contains("SW_SPDT"));
        assert!(py.contains("LED_GENERIC"));
        assert!(py.contains(r#"jack_in += j_in["T"], sw_in["B"]"#));
        assert!(py.contains(r#"vout += rv2[2]"#));
        assert!(py.contains(r#"q1["B"]"#));
        assert!(py.contains("fuzz_b"));
        assert!(!py.contains("q2"));
    }

    #[test]
    fn render_chain_skidl_wires_a_second_stage_with_a_fixed_emitter() {
        let chain = FuzzChain {
            vcc: 9.0,
            ic_ma: 0.5,
            enclosure_size: EnclosureSize::Size1590B,
            stages: vec![
                GainStage {
                    bias_voice: BiasVoice::Symmetric,
                    gain_character: GainCharacter::Balanced,
                },
                GainStage {
                    bias_voice: BiasVoice::BrightAsymmetric,
                    gain_character: GainCharacter::Tame,
                },
            ],
            tone_stack: true,
        };
        let py = render_chain_skidl(&chain);
        assert!(py.contains(r#"q2["B"]"#));
        assert!(py.contains("Q2_EMITTER"));
        assert!(py.contains("tone_node"));
        // Only stage 1 gets the fuzz pot / rheostat treatment.
        assert_eq!(py.matches("R_Potentiometer").count(), 2); // fuzz pot + volume pot
    }
}
