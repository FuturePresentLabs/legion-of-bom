//! Concept → spec → design, kept as three genuinely separate artifacts.
//!
//! **Spec** ([`generate_fuzz_pedal_spec`]) asks every typed [`crate::decision`]
//! call once and produces a [`FuzzPedalSpec`] — decisions in, nothing else.
//! It is renderable as raw text ([`render_spec_text`], for a human to read) or
//! as JSON (`FuzzPedalSpec` is `Serialize`/`Deserialize`, for a machine to
//! replay). It contains no schematic.
//!
//! **Design** ([`render_skidl`]) is a *pure function* of a [`FuzzPedalSpec`] —
//! no decision calls, no network. `lob schematic` runs it against a saved
//! spec file. This is the literal sense in which the schematic is "built via
//! the SystemOne decisions": every value in it traces back to exactly the
//! typed answers System One gave, replayable from the spec file with no
//! re-querying (which could answer differently the second time) and no
//! independent judgment call sneaking in between spec and design.
//!
//! Per DESIGN.md §3.4/§7.9's "curated, not generated" stance, schematic
//! *selection* is a bounded `choice` over a small, human-curated topology
//! library — today one entry — never an open request to invent a circuit
//! graph. Only the free parameters *within* a chosen topology (bias voicing,
//! gain character, whether to include a tone network, enclosure size) are
//! decided per run. System One cannot emit code in any case — it returns a
//! choice, a probability, or a rubric position, never free text — so "build
//! the schematic via the decisions" necessarily means: every structural
//! degree of freedom this module exposes is its own typed decision, and the
//! renderer's job is only to assemble what was decided, not to decide
//! anything itself.
//!
//! **First-pass, not gospel.** This is the "reasonable guess/plan" the concept
//! → spec → design pipeline exists to produce — resistor values come from
//! real bias-point arithmetic (never memorized from a specific commercial
//! schematic), rounded to the nearest E12 value. The transistor's symbol pin
//! → package pin assignment is a first-pass guess and is NOT `verified_by_human`
//! — `lob parts gate` still blocks real board/BOM generation on it, same as
//! any other unverified MPN (DESIGN.md §3.5/§4.3).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::decision::{Answer, DecisionClient, DecisionError, DecisionRecord, NamedQuestion};

/// Errors generating a spec.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("decision failed: {0}")]
    Decision(#[from] DecisionError),
}

/// The curated topology library. One entry today — extensible, per DESIGN.md
/// §3.4's "requirement lives with the role" pattern applied to schematics: the
/// *set* of valid topologies is curated by a human, the pipeline only ever
/// picks among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Topology {
    /// Two-stage NPN silicon common-emitter fuzz, RC-coupled, Fuzz-Face-family
    /// inspired (not a reproduction of any specific historic schematic).
    Silicon2TransistorFuzz,
}

impl Topology {
    fn key(self) -> &'static str {
        match self {
            Topology::Silicon2TransistorFuzz => "silicon_2t_fuzz",
        }
    }

    const CURATED: &'static [(&'static str, &'static str)] = &[(
        "silicon_2t_fuzz",
        "Two-stage NPN silicon common-emitter fuzz, RC-coupled between \
         stages, clipping via transistor rail-clipping (not diodes) — the \
         Fuzz-Face family's mechanism, not a specific historic schematic.",
    )];
}

/// How hard stage 2's collector bias sits off center — the clipping-asymmetry
/// control. Stage 1 always biases near center; stage 2 carries the "voice".
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum BiasVoice {
    /// Collector at ~35% VCC: clips the positive half sooner — brighter, more
    /// aggressive top end.
    BrightAsymmetric,
    /// Collector at ~50% VCC: roughly even clipping on both halves.
    Symmetric,
    /// Collector at ~65% VCC: clips the negative half sooner — darker, fuzzier.
    DarkAsymmetric,
}

impl BiasVoice {
    fn vc_fraction(self) -> f64 {
        match self {
            BiasVoice::BrightAsymmetric => 0.35,
            BiasVoice::Symmetric => 0.5,
            BiasVoice::DarkAsymmetric => 0.65,
        }
    }

    const CHOICES: &'static [(&'static str, &'static str)] = &[
        (
            "bright_asymmetric",
            "Stage-2 collector biased ~35% VCC — clips the positive half sooner, brighter/more aggressive top end",
        ),
        (
            "symmetric",
            "Stage-2 collector biased ~50% VCC — roughly even clipping on both halves",
        ),
        (
            "dark_asymmetric",
            "Stage-2 collector biased ~65% VCC — clips the negative half sooner, darker/fuzzier",
        ),
    ];

    fn from_key(key: &str) -> BiasVoice {
        match key {
            "bright_asymmetric" => BiasVoice::BrightAsymmetric,
            "dark_asymmetric" => BiasVoice::DarkAsymmetric,
            _ => BiasVoice::Symmetric,
        }
    }
}

/// How much available gain the fuzz pot's floor resistor leaves on the table —
/// the "how unstable can this get" control. Lower floor = higher achievable
/// gain (less emitter degeneration at full clockwise).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum GainCharacter {
    Tame,
    Balanced,
    Aggressive,
    Unstable,
}

impl GainCharacter {
    const LEVELS: &'static [&'static str] = &["tame", "balanced", "aggressive", "unstable"];

    fn floor_ohms(self) -> f64 {
        match self {
            GainCharacter::Tame => 680.0,
            GainCharacter::Balanced => 330.0,
            GainCharacter::Aggressive => 150.0,
            GainCharacter::Unstable => 47.0,
        }
    }

    /// Snap a `score` rubric position (0-indexed into [`Self::LEVELS`]) to the
    /// nearest level — deterministic, no interpolation.
    fn from_score(score: f64) -> GainCharacter {
        match score.round().clamp(0.0, 3.0) as i64 {
            0 => GainCharacter::Tame,
            1 => GainCharacter::Balanced,
            2 => GainCharacter::Aggressive,
            _ => GainCharacter::Unstable,
        }
    }
}

/// Standard stompbox enclosure size classes (DESIGN.md §7.7's already-named
/// set) — stored here for Milestone 3's `PanelSpec`, not used by the SKiDL
/// renderer itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnclosureSize {
    /// Hammond 1590B — 112 x 60 x 31 mm, the standard single-footswitch pedal size.
    Size1590B,
    /// Hammond 1590BB — 120 x 94 x 34 mm, room for more controls.
    Size1590BB,
    /// Hammond 125B — 125 x 66 x 39 mm, standard depth, more room than 1590B.
    Size125B,
}

impl EnclosureSize {
    const CHOICES: &'static [(&'static str, &'static str)] = &[
        (
            "1590B",
            "112 x 60 x 31 mm — the standard single-footswitch pedal size, tightest fit",
        ),
        (
            "1590BB",
            "120 x 94 x 34 mm — room for more controls than 1590B",
        ),
        (
            "125B",
            "125 x 66 x 39 mm — standard depth, more internal room than 1590B",
        ),
    ];

    fn from_key(key: &str) -> EnclosureSize {
        match key {
            "1590BB" => EnclosureSize::Size1590BB,
            "125B" => EnclosureSize::Size125B,
            _ => EnclosureSize::Size1590B,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            EnclosureSize::Size1590B => "1590B",
            EnclosureSize::Size1590BB => "1590BB",
            EnclosureSize::Size125B => "125B",
        }
    }
}

/// Nearest-E12-preferred-value rounding, applied to every computed bias
/// resistor so the spec renders real, orderable component values rather than
/// exact-but-unbuyable arithmetic results.
const E12: [f64; 12] = [1.0, 1.2, 1.5, 1.8, 2.2, 2.7, 3.3, 3.9, 4.7, 5.6, 6.8, 8.2];

fn round_e12(value: f64) -> f64 {
    if value <= 0.0 {
        return 0.0;
    }
    let decade = value.log10().floor();
    let mantissa = value / 10f64.powf(decade);
    let nearest = E12
        .iter()
        .copied()
        .min_by(|a, b| {
            (a - mantissa)
                .abs()
                .partial_cmp(&(b - mantissa).abs())
                .unwrap()
        })
        .unwrap();
    nearest * 10f64.powf(decade)
}

/// Format an ohm value the way the rest of this repo's circuits do (`"9k"`,
/// `"680"`, `"1.5M"`) — SKiDL/KiCad accept engineering suffixes directly.
fn fmt_ohms(v: f64) -> String {
    if v >= 1e6 {
        format!("{}M", trim_trailing_zero(v / 1e6))
    } else if v >= 1e3 {
        format!("{}k", trim_trailing_zero(v / 1e3))
    } else {
        trim_trailing_zero(v)
    }
}

fn trim_trailing_zero(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Common-emitter voltage-divider bias, computed from first principles (not
/// recalled from a specific published schematic) — see the module docs.
/// Returns `(rc_ohms, r_top_ohms, r_bottom_ohms)`, each already E12-rounded.
fn bias_network(
    vcc: f64,
    vc_fraction: f64,
    ic_ma: f64,
    re_ohms: f64,
    beta: f64,
) -> (f64, f64, f64) {
    let ic_a = ic_ma / 1000.0;
    let vc = vcc * vc_fraction;
    let rc = round_e12((vcc - vc) / ic_a);

    let ve = ic_a * re_ohms;
    let vb = ve + VBE_V;
    let ib = ic_a / beta;
    let idiv = DIV_CURRENT_MULT * ib;
    let r_bottom = round_e12(vb / idiv);
    let r_top = round_e12((vcc - vb) / idiv);
    (rc, r_top, r_bottom)
}

const VCC_V: f64 = 9.0;
const IC_MA: f64 = 0.5;
const BETA: f64 = 200.0;
const VBE_V: f64 = 0.6;
/// Divider-current multiple of base current — keeps the bias point stiff
/// against hFE spread without wasting excessive battery current.
const DIV_CURRENT_MULT: f64 = 20.0;

const FUZZ_POT_MAX_OHMS: f64 = 500.0;
const STAGE2_EMITTER_OHMS: f64 = 1000.0;

/// Every value chosen for one fuzz-pedal design run, plus the resistor/cap
/// values derived from them. `Serialize`/`Deserialize` so a spec is a real
/// artifact: `lob spec` writes it, `lob schematic` reads it back and renders
/// from exactly what's in the file, no fresh decisions involved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuzzPedalSpec {
    pub topology: Topology,
    voice: BiasVoice,
    character: GainCharacter,
    pub tone_stack: bool,
    pub enclosure_size: EnclosureSize,

    // Derived component values (ohms unless noted), already E12-rounded.
    stage1_rc: f64,
    stage1_r_top: f64,
    stage1_r_bottom: f64,
    stage1_fuzz_floor: f64,
    stage2_rc: f64,
    stage2_r_top: f64,
    stage2_r_bottom: f64,
}

/// Pull a `choice` answer out of a batched [`DecisionClient::ask_many`]
/// result by key, erroring loud (never guessing) if it's missing or came
/// back as the wrong answer kind — which would mean this module's own
/// question-building code is broken, not that the caller did anything wrong.
fn expect_choice(answers: &mut HashMap<String, Answer>, key: &str) -> Result<String, SpecError> {
    match answers.remove(key) {
        Some(Answer::Choice(a)) => Ok(a.choice),
        _ => Err(SpecError::Decision(DecisionError::MalformedAnswer(
            key.to_string(),
        ))),
    }
}

/// Pull a `noul` answer by key — see [`expect_choice`].
fn expect_noul(answers: &mut HashMap<String, Answer>, key: &str) -> Result<f64, SpecError> {
    match answers.remove(key) {
        Some(Answer::Noul(v)) => Ok(v),
        _ => Err(SpecError::Decision(DecisionError::MalformedAnswer(
            key.to_string(),
        ))),
    }
}

/// Pull a `score` answer's score value by key — see [`expect_choice`].
fn expect_score(answers: &mut HashMap<String, Answer>, key: &str) -> Result<f64, SpecError> {
    match answers.remove(key) {
        Some(Answer::Score(a)) => Ok(a.score),
        _ => Err(SpecError::Decision(DecisionError::MalformedAnswer(
            key.to_string(),
        ))),
    }
}

/// Ask every typed decision for a fuzz-pedal spec and derive its component
/// values. `brief` is the free-text design brief (e.g. "vintage silicon fuzz,
/// 9V, true bypass") — carried as `state` context for every question, never
/// parsed for control flow itself (the *decisions*, not the brief text,
/// determine the circuit).
pub fn generate_fuzz_pedal_spec(
    client: &mut DecisionClient,
    brief: &str,
) -> Result<FuzzPedalSpec, SpecError> {
    // All five decisions go in one batched call (legion-of-bom-x74e) instead
    // of five round trips -- none of them depends on another's answer yet
    // (the curated topology set is one entry, so nothing branches off it),
    // so batching is a pure latency win with nothing to design around.
    let mut answers = client.ask_many(
        brief,
        vec![
            // Curated set has one entry today; the choice call still runs
            // (and produces a real trace/confidence entry) so a second
            // topology is a new catalog entry, not a new code path, once one
            // exists.
            NamedQuestion::choice(
                "topology",
                "Pick the circuit topology for this fuzz pedal from the curated set.",
                Topology::CURATED,
            ),
            NamedQuestion::choice(
                "bias_voice",
                "Pick how stage 2's collector bias sits off center, which sets the \
                 clipping-asymmetry character.",
                BiasVoice::CHOICES,
            ),
            NamedQuestion::score(
                "gain_character",
                "Rate how much gain/instability this fuzz should make available at \
                 full clockwise on the Fuzz control.",
                GainCharacter::LEVELS,
            ),
            NamedQuestion::noul(
                "tone_stack",
                "Should this design include a simple fixed treble-cut tone network \
                 ahead of the volume control?",
                "yes, include a simple tone network",
                "no, direct output to volume",
            ),
            NamedQuestion::choice(
                "enclosure_size",
                "Pick the enclosure size class for this pedal.",
                EnclosureSize::CHOICES,
            ),
        ],
    )?;

    let topology = Topology::Silicon2TransistorFuzz;
    let voice = BiasVoice::from_key(&expect_choice(&mut answers, "bias_voice")?);
    let character = GainCharacter::from_score(expect_score(&mut answers, "gain_character")?);
    let tone_stack = expect_noul(&mut answers, "tone_stack")? >= 0.5;
    let enclosure_size = EnclosureSize::from_key(&expect_choice(&mut answers, "enclosure_size")?);

    let (stage1_rc, stage1_r_top, stage1_r_bottom) = bias_network(
        VCC_V,
        0.5,
        IC_MA,
        character.floor_ohms() + FUZZ_POT_MAX_OHMS / 2.0,
        BETA,
    );
    let (stage2_rc, stage2_r_top, stage2_r_bottom) =
        bias_network(VCC_V, voice.vc_fraction(), IC_MA, STAGE2_EMITTER_OHMS, BETA);

    Ok(FuzzPedalSpec {
        topology,
        voice,
        character,
        tone_stack,
        enclosure_size,
        stage1_rc,
        stage1_r_top,
        stage1_r_bottom,
        stage1_fuzz_floor: round_e12(character.floor_ohms()),
        stage2_rc,
        stage2_r_top,
        stage2_r_bottom,
    })
}

/// Render a [`FuzzPedalSpec`] as a raw-text specification document — the
/// actual "spec" artifact `lob spec` writes. No schematic, no code: what was
/// decided, at what confidence, and what it implies, so a human can read it
/// (or route it to `lob schematic`) without opening the JSON.
pub fn render_spec_text(brief: &str, spec: &FuzzPedalSpec, trace: &[DecisionRecord]) -> String {
    let mut out = String::new();
    out.push_str("FUZZ PEDAL SPEC\n");
    out.push_str("===============\n\n");
    out.push_str(&format!("Brief: {brief}\n\n"));
    out.push_str("Decisions (System One / Jev-compatible, ai.fpl.dev/v1/systemone):\n");
    for record in trace {
        out.push_str(&format!(
            "  - {:<16} [{}] {} (confidence {:.2})\n",
            record.key, record.kind, record.chosen, record.confidence
        ));
    }
    out.push_str("\nResolved design:\n");
    out.push_str(&format!("  topology       = {}\n", spec.topology.key()));
    out.push_str(&format!(
        "  bias_voice     = {:?} (stage-2 collector at {:.0}% VCC)\n",
        spec.voice,
        spec.voice.vc_fraction() * 100.0
    ));
    out.push_str(&format!(
        "  gain_character = {:?} (fuzz-pot floor {} ohm)\n",
        spec.character,
        fmt_ohms(spec.stage1_fuzz_floor)
    ));
    out.push_str(&format!("  tone_stack     = {}\n", spec.tone_stack));
    out.push_str(&format!(
        "  enclosure_size = {}\n",
        spec.enclosure_size.key()
    ));
    out.push_str("\nDerived component values (E12-rounded, VCC=9V, Ic=0.5mA per stage):\n");
    out.push_str(&format!(
        "  stage 1: Rtop={} Rbottom={} Rc={}\n",
        fmt_ohms(spec.stage1_r_top),
        fmt_ohms(spec.stage1_r_bottom),
        fmt_ohms(spec.stage1_rc)
    ));
    out.push_str(&format!(
        "  stage 2: Rtop={} Rbottom={} Rc={}\n",
        fmt_ohms(spec.stage2_r_top),
        fmt_ohms(spec.stage2_r_bottom),
        fmt_ohms(spec.stage2_rc)
    ));
    out.push_str(
        "\nThis file contains no schematic. `lob schematic <this-spec>.json --out \
         <path>.py` renders the SKiDL circuit from exactly these decisions, with no \
         further System One calls.\n",
    );
    out
}

/// Render a [`FuzzPedalSpec`] as a SKiDL circuit script, following the same
/// shape every hand-written circuit in `examples/` uses (`Part`/`Net`/`ERC`/
/// `generate_netlist`, a `build()` function, `Sim.*` fields on active parts) —
/// see `examples/opamp_noninv.py`.
pub fn render_skidl(spec: &FuzzPedalSpec) -> String {
    let tone_block = if spec.tone_stack {
        r#"
    # Simple fixed treble-cut tone network ahead of the volume pot: a series
    # R-C to ground (not R alone, and not R||C -- both would load the signal
    # at every frequency instead of only rolling off the highs).
    r_tone = Part("Device", "R", value="4.7k", footprint=R_FOOTPRINT, ref="R5")
    c_tone = Part("Device", "C", value="10n", footprint=C_FOOTPRINT, ref="C4")
    tone_node = Net("TONE")
    tone_mid = Net("TONE_MID")
    tone_node += c_out[2], r_tone[1]
    tone_mid += r_tone[2], c_tone[1]
    gnd += c_tone[2]
    vol_in = tone_node
"#
    } else {
        r#"
    vol_in = c_out[2]
"#
    };

    format!(
        r#""""{topology} — first-pass concept-to-spec output (lob spec fuzz-pedal).

NOT a reproduction of a specific historic schematic — a Fuzz-Face-family-
inspired two-stage NPN silicon common-emitter fuzz, RC-coupled between
stages. Clips by transistor rail-clipping (not diodes), same mechanism real
fuzz circuits in this family use. Bias resistors are computed from first-
principles bias-point arithmetic (Ic={ic_ma} mA, VCC={vcc}V, beta={beta} assumed
for divider sizing), rounded to the nearest E12 value — not recalled from any
specific published schematic.

Decisions baked into this run (see the sibling .decisions.json trace):
  bias_voice      = {voice:?}  (stage-2 collector at {vc_frac:.0}% VCC)
  gain_character  = {character:?}  (fuzz-pot floor {floor} ohm)
  tone_stack      = {tone_stack}
  enclosure_size  = {enclosure}

Q1/Q2 use KiCad's generic Device:Q_NPN symbol, whose pins are letter-named
(B/C/E, not numbered) — read directly from the installed
Device.kicad_sym, not recalled from memory. **What's still UNVERIFIED is the
MMBT3904 footprint's physical lead order** (which SOT-23 pad is actually
base/emitter/collector on the real part) — a first-pass guess, not a
verified fact. `lob parts gate` will (correctly) block real board/BOM
generation on this MPN until a human confirms it against the datasheet, same
as any other unverified part (DESIGN.md 3.5/4.3).

Power/ground/jack wiring, the footswitch, and panel layout are NOT modelled
here — this file is the audio signal path only, checked by lob run (ERC +
ngspice); panel/mechanical layout is a separate stage.

Run standalone (needs KICAD9_SYMBOL_DIR); `lob run`/`lob board` set it up.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

R_FOOTPRINT = "Resistor_SMD:R_0805_2012Metric"
C_FOOTPRINT = "Capacitor_SMD:C_0805_2012Metric"
Q_FOOTPRINT = "Package_TO_SOT_SMD:SOT-23"
Q_MPN = "MMBT3904"  # NPN silicon, SOT-23 -- footprint lead order unverified, see docstring

FUZZ_POT_FLOOR = "{fuzz_floor}"  # fixed floor in series with the Fuzz pot (unbypassed emitter)
FUZZ_POT_MAX = "{fuzz_pot_max}"


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
    """Construct the fuzz circuit in the default SKiDL circuit."""
    q1 = _npn("Q1")
    q2 = _npn("Q2")

    # Stage 1 bias network (Q1): base divider + collector resistor.
    r1_top = Part("Device", "R", value="{s1_top}", footprint=R_FOOTPRINT, ref="R1")
    r1_bottom = Part("Device", "R", value="{s1_bottom}", footprint=R_FOOTPRINT, ref="R2")
    r1_c = Part("Device", "R", value="{s1_rc}", footprint=R_FOOTPRINT, ref="R3")

    # Fuzz control: unbypassed emitter resistance in Q1's path. Fixed floor
    # (sets the character's gain ceiling) in series with the Fuzz pot, wired
    # as a rheostat (wiper shorted to one end) for a monotonic taper.
    r1_floor = Part("Device", "R", value=FUZZ_POT_FLOOR, footprint=R_FOOTPRINT, ref="R4")
    p_fuzz = Part("Device", "R_Potentiometer", value=FUZZ_POT_MAX, footprint="Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical", ref="RV1")

    # Stage 2 bias network (Q2).
    r2_top = Part("Device", "R", value="{s2_top}", footprint=R_FOOTPRINT, ref="R6")
    r2_bottom = Part("Device", "R", value="{s2_bottom}", footprint=R_FOOTPRINT, ref="R7")
    r2_c = Part("Device", "R", value="{s2_rc}", footprint=R_FOOTPRINT, ref="R8")
    r2_e = Part("Device", "R", value="1k", footprint=R_FOOTPRINT, ref="R9")

    # Coupling caps.
    c_in = Part("Device", "C", value="2.2u", footprint=C_FOOTPRINT, ref="C1")
    c_inter = Part("Device", "C", value="100n", footprint=C_FOOTPRINT, ref="C2")
    c_out = Part("Device", "C", value="1u", footprint=C_FOOTPRINT, ref="C3")

    # Output volume: standard voltage-divider wiring (wiper = pedal OUT).
    p_vol = Part("Device", "R_Potentiometer", value="100k", footprint="Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical", ref="RV2")

    vin = Net("IN")
    vout = Net("OUT")
    gnd = Net("GND")
    vcc = Net("+9V")

    # -- Stage 1: Q1 common-emitter, Fuzz-pot-controlled emitter feedback.
    base1 = Net("Q1_BASE")
    vin += c_in[1]
    base1 += c_in[2], r1_top[2], r1_bottom[1], q1["B"]
    vcc += r1_top[1]
    gnd += r1_bottom[2]
    fuzz_a = Net("FUZZ_A")
    q1["E"] += fuzz_a
    fuzz_a += r1_floor[1]
    fuzz_b = Net("FUZZ_B")
    r1_floor[2] += fuzz_b
    # Rheostat wiring: pin 2 is the wiper (this project's SPICE-deck
    # convention, crate::spice's pot handling -- "1-wiper(2)-3"), tied to the
    # unused end (pin 3) as a noise-immunity safety tie, a standard practice
    # for a rheostat-configured pot. Pin 1 is the far end that actually
    # varies the resistance.
    fuzz_b += p_fuzz[1]
    gnd += p_fuzz[2], p_fuzz[3]
    stage1_out = Net("STAGE1_OUT")
    stage1_out += q1["C"], r1_c[1]
    vcc += r1_c[2]

    # -- Interstage coupling.
    base2 = Net("Q2_BASE")
    stage1_out += c_inter[1]
    base2 += c_inter[2], r2_top[2], r2_bottom[1], q2["B"]
    vcc += r2_top[1]
    gnd += r2_bottom[2]

    # -- Stage 2: Q2 common-emitter, fixed emitter resistor sets the voice.
    e2 = Net("Q2_EMITTER")
    e2 += q2["E"], r2_e[1]
    gnd += r2_e[2]
    stage2_out = Net("STAGE2_OUT")
    stage2_out += q2["C"], r2_c[1]
    vcc += r2_c[2]

    # -- Output coupling + optional tone + volume.
    stage2_out += c_out[1]
{tone_block}
    # Standard volume-divider wiring: pin 2 is the wiper (see the Fuzz pot's
    # comment above for why), so it -- not pin 3 -- is the output.
    p_vol[1] += vol_in
    vout += p_vol[2]  # wiper
    gnd += p_vol[3]

    return {{
        "Q1": q1, "Q2": q2,
        "R1": r1_top, "R2": r1_bottom, "R3": r1_c, "R4": r1_floor,
        "R6": r2_top, "R7": r2_bottom, "R8": r2_c, "R9": r2_e,
        "RV1": p_fuzz, "RV2": p_vol,
        "C1": c_in, "C2": c_inter, "C3": c_out,
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
        topology = spec.topology.key(),
        ic_ma = IC_MA,
        vcc = VCC_V,
        beta = BETA,
        voice = spec.voice,
        vc_frac = spec.voice.vc_fraction() * 100.0,
        character = spec.character,
        floor = fmt_ohms(spec.stage1_fuzz_floor),
        tone_stack = spec.tone_stack,
        enclosure = spec.enclosure_size.key(),
        fuzz_floor = fmt_ohms(spec.stage1_fuzz_floor),
        fuzz_pot_max = fmt_ohms(FUZZ_POT_MAX_OHMS),
        s1_top = fmt_ohms(spec.stage1_r_top),
        s1_bottom = fmt_ohms(spec.stage1_r_bottom),
        s1_rc = fmt_ohms(spec.stage1_rc),
        s2_top = fmt_ohms(spec.stage2_r_top),
        s2_bottom = fmt_ohms(spec.stage2_r_bottom),
        s2_rc = fmt_ohms(spec.stage2_rc),
        tone_block = tone_block,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub the decision client can't reach in `cargo test` — exercised
    /// through the real `DecisionClient` type but never actually calling out,
    /// by testing the pure functions this module is built from directly.
    #[test]
    fn e12_rounds_to_nearest_preferred_value() {
        assert_eq!(round_e12(9000.0), 8200.0);
        assert_eq!(round_e12(17800.0), 18000.0);
        assert_eq!(round_e12(22000.0), 22000.0);
    }

    #[test]
    fn fmt_ohms_uses_engineering_suffixes() {
        assert_eq!(fmt_ohms(680.0), "680");
        assert_eq!(fmt_ohms(8200.0), "8.2k");
        assert_eq!(fmt_ohms(150000.0), "150k");
        assert_eq!(fmt_ohms(1_500_000.0), "1.5M");
    }

    #[test]
    fn bias_network_produces_sane_component_values() {
        let (rc, top, bottom) = bias_network(9.0, 0.5, 0.5, 580.0, 200.0);
        // Collector resistor should sit VCC/2 above ground at 0.5 mA: ~9k.
        assert!((rc - 9000.0).abs() < 2000.0, "rc={rc}");
        // Divider resistors should be in the tens-to-hundreds-of-kohm range,
        // not negative or absurdly large.
        assert!(top > 0.0 && top < 1_000_000.0, "top={top}");
        assert!(bottom > 0.0 && bottom < 100_000.0, "bottom={bottom}");
    }

    #[test]
    fn gain_character_snaps_score_to_nearest_level() {
        assert_eq!(GainCharacter::from_score(0.2), GainCharacter::Tame);
        assert_eq!(GainCharacter::from_score(1.4), GainCharacter::Balanced);
        assert_eq!(GainCharacter::from_score(2.4), GainCharacter::Aggressive);
        assert_eq!(GainCharacter::from_score(3.9), GainCharacter::Unstable);
    }

    #[test]
    fn enclosure_size_round_trips_through_key() {
        for (key, _) in EnclosureSize::CHOICES {
            let size = EnclosureSize::from_key(key);
            assert_eq!(size.key(), *key);
        }
    }

    #[test]
    fn render_skidl_produces_parseable_python_shape() {
        let spec = FuzzPedalSpec {
            topology: Topology::Silicon2TransistorFuzz,
            voice: BiasVoice::Symmetric,
            character: GainCharacter::Balanced,
            tone_stack: true,
            enclosure_size: EnclosureSize::Size1590B,
            stage1_rc: 8200.0,
            stage1_r_top: 150000.0,
            stage1_r_bottom: 18000.0,
            stage1_fuzz_floor: 330.0,
            stage2_rc: 8200.0,
            stage2_r_top: 150000.0,
            stage2_r_bottom: 22000.0,
        };
        let py = render_skidl(&spec);
        assert!(py.contains("def build():"));
        assert!(py.contains("def main(argv=None):"));
        assert!(py.contains("generate_netlist(file_=args.output)"));
        assert!(py.contains("Q_NPN"));
        assert!(py.contains(r#"q1["B"]"#));
        assert!(py.contains("r_tone")); // tone_stack=true branch rendered
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = FuzzPedalSpec {
            topology: Topology::Silicon2TransistorFuzz,
            voice: BiasVoice::DarkAsymmetric,
            character: GainCharacter::Aggressive,
            tone_stack: true,
            enclosure_size: EnclosureSize::Size1590BB,
            stage1_rc: 8200.0,
            stage1_r_top: 150000.0,
            stage1_r_bottom: 18000.0,
            stage1_fuzz_floor: 150.0,
            stage2_rc: 6800.0,
            stage2_r_top: 150000.0,
            stage2_r_bottom: 22000.0,
        };
        let json = serde_json::to_string_pretty(&spec).unwrap();
        let back: FuzzPedalSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn render_spec_text_contains_no_schematic_but_names_the_next_step() {
        let spec = FuzzPedalSpec {
            topology: Topology::Silicon2TransistorFuzz,
            voice: BiasVoice::Symmetric,
            character: GainCharacter::Balanced,
            tone_stack: true,
            enclosure_size: EnclosureSize::Size1590B,
            stage1_rc: 8200.0,
            stage1_r_top: 150000.0,
            stage1_r_bottom: 18000.0,
            stage1_fuzz_floor: 330.0,
            stage2_rc: 8200.0,
            stage2_r_top: 150000.0,
            stage2_r_bottom: 22000.0,
        };
        let trace = vec![DecisionRecord {
            key: "topology".into(),
            kind: "choice".into(),
            chosen: "silicon_2t_fuzz".into(),
            confidence: 0.97,
            timestamp_unix: 0,
        }];
        let text = render_spec_text("vintage silicon fuzz, 9V", &spec, &trace);
        assert!(text.contains("vintage silicon fuzz, 9V"));
        assert!(text.contains("silicon_2t_fuzz"));
        assert!(text.contains("lob schematic"));
        assert!(!text.contains("def build()"));
        assert!(!text.contains("import skidl"));
    }

    #[test]
    fn render_skidl_omits_tone_block_when_disabled() {
        let mut spec = FuzzPedalSpec {
            topology: Topology::Silicon2TransistorFuzz,
            voice: BiasVoice::Symmetric,
            character: GainCharacter::Balanced,
            tone_stack: false,
            enclosure_size: EnclosureSize::Size1590B,
            stage1_rc: 8200.0,
            stage1_r_top: 150000.0,
            stage1_r_bottom: 18000.0,
            stage1_fuzz_floor: 330.0,
            stage2_rc: 8200.0,
            stage2_r_top: 150000.0,
            stage2_r_bottom: 22000.0,
        };
        spec.tone_stack = false;
        let py = render_skidl(&spec);
        assert!(!py.contains("r_tone"));
        assert!(py.contains("vol_in = c_out[2]"));
    }
}
