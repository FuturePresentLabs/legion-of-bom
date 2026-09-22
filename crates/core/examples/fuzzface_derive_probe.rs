//! [`fuzzface_quiz_probe`] showed the endpoint is confidently right about the
//! Fuzz Face's *topology* but honestly unsure when asked to bare-recall an
//! exact resistor value (0.08 confidence on "what's Q1's collector
//! resistor?"). That's the wrong question to ask a model at all — it's asking
//! for memorized trivia where real engineering reasoning exists instead.
//!
//! This asks for the two *physical* quantities a real engineer would reason
//! from — Q1's collector bias voltage and quiescent current, both bounded
//! choices over plausible bands, not a number pulled from memory — then
//! DERIVES Q1's collector resistor with real Ohm's law:
//!
//!   R = (|Vsupply| - |Vc1|) / Ic1
//!
//! exactly the same "compute from what's known, don't hardcode" move this
//! crate's own [`crate::topology`] module already makes for VCC/IC. The
//! derived value is then checked against the real, sourced answer (33k,
//! same provenance as [`fuzzface_quiz_probe`]) — not because the model
//! recalled it, but because it should fall out of physics the model DOES
//! seem to hold (Keen's article: Q1's collector biases only ~0.5V off
//! ground; confirmed again here as a band, not a single guess).
//!
//! ```text
//! cargo run -p legion-of-bom-core --example fuzzface_derive_probe
//! ```

use ooda::{Client, Criteria, Question, Request};

/// Real Fuzz Face supply magnitude (9V, positive-ground / -9V rail — see
/// [`fuzzface_quiz_probe`]'s `supply_convention` question). The sign doesn't
/// matter for this Ohm's-law derivation, only the magnitude across R.
const SUPPLY_V: f64 = 9.0;

/// The real, sourced value ([`fuzzface_quiz_probe`]'s `q1_collector_resistor`
/// answer key) this derivation is checked against.
const REAL_R_C1_OHMS: f64 = 33_000.0;

struct Band {
    key: &'static str,
    description: &'static str,
    mid: f64,
}

const VC1_BANDS: &[Band] = &[
    Band { key: "0.1_0.3v", description: "about 0.1-0.3V off ground", mid: 0.2 },
    Band { key: "0.4_0.6v", description: "about 0.4-0.6V off ground", mid: 0.5 },
    Band { key: "0.8_1.2v", description: "about 0.8-1.2V off ground", mid: 1.0 },
    Band { key: "2_3v", description: "about 2-3V off ground", mid: 2.5 },
];

const IC1_BANDS: &[Band] = &[
    Band { key: "10_50ua", description: "about 10-50 microamps", mid: 30e-6 },
    Band { key: "50_150ua", description: "about 50-150 microamps", mid: 100e-6 },
    Band { key: "150_350ua", description: "about 150-350 microamps", mid: 250e-6 },
    Band { key: "350ua_1ma", description: "about 350 microamps to 1 milliamp", mid: 650e-6 },
];

fn find_mid<'a>(bands: &'a [Band], key: &str) -> Option<&'a Band> {
    bands.iter().find(|b| b.key == key)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ooda::HttpClient::from_env()
        .map_err(|e| format!("{e} (need OODA_API_KEY — see .env.example)"))?;

    let observation = serde_json::json!({
        "topic": "Q1 (the first transistor) in a classic, correctly biased Dallas Arbiter Fuzz Face — PNP germanium, positive-ground, -9V supply. Q1's emitter is grounded directly, collector loaded by a single resistor to the -9V rail.",
        "task": "Answer from real engineering knowledge of this specific circuit's bias point, not a guess.",
    });

    let vc1_criteria: Criteria = VC1_BANDS.iter().map(|b| (b.key, b.description)).collect();
    let ic1_criteria: Criteria = IC1_BANDS.iter().map(|b| (b.key, b.description)).collect();

    let request = Request::new(observation)
        .with(
            "vc1_band",
            Question::choice(
                "How far off ground does Q1's collector sit when the circuit is correctly biased (magnitude, ignoring sign)?",
                vc1_criteria,
            ),
        )
        .with(
            "ic1_band",
            Question::choice(
                "What quiescent collector current does Q1 run at, typical for a low-power germanium small-signal stage biased this way?",
                ic1_criteria,
            ),
        );

    println!("fuzzface_derive_probe: deriving Q1's collector resistor via Ohm's law from 2 bounded physical decisions\n");
    let outcome = client.decide(&request)?;

    let vc1_answer = outcome.answer("vc1_band")?;
    let ic1_answer = outcome.answer("ic1_band")?;
    let vc1_key = vc1_answer.choice().unwrap_or("?");
    let ic1_key = ic1_answer.choice().unwrap_or("?");
    let vc1_band = find_mid(VC1_BANDS, vc1_key);
    let ic1_band = find_mid(IC1_BANDS, ic1_key);

    println!(
        "  vc1_band: {:<10} ({})  confidence={:.2}",
        vc1_key,
        vc1_band.map(|b| b.description).unwrap_or("?"),
        vc1_answer.confidence().unwrap_or(f64::NAN)
    );
    println!(
        "  ic1_band: {:<10} ({})  confidence={:.2}\n",
        ic1_key,
        ic1_band.map(|b| b.description).unwrap_or("?"),
        ic1_answer.confidence().unwrap_or(f64::NAN)
    );

    match (vc1_band, ic1_band) {
        (Some(vc1), Some(ic1)) => {
            let derived_r = (SUPPLY_V - vc1.mid) / ic1.mid;
            let ratio = derived_r / REAL_R_C1_OHMS;
            println!(
                "  R = (|Vsupply| - |Vc1|) / Ic1 = ({SUPPLY_V} - {}) / {:.6} = {derived_r:.0} ohm",
                vc1.mid, ic1.mid
            );
            println!(
                "  real (sourced) value: {REAL_R_C1_OHMS:.0} ohm  |  derived/real ratio: {ratio:.2}x{}",
                if (0.5..=2.0).contains(&ratio) {
                    "  <<< within 2x — same engineering ballpark as the real part"
                } else {
                    "  <<< off by more than 2x"
                }
            );
        }
        _ => println!("  could not match returned bands to a known key"),
    }

    Ok(())
}
