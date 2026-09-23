//! Not "can legion-of-bom render a Fuzz Face" but "does the Jev/Laya decision
//! endpoint actually KNOW the real one" — a bounded multiple-choice quiz about
//! the classic Dallas Arbiter Fuzz Face's real, well-documented topology and
//! component values, put to the same typed-decision wire (`ooda::Client`,
//! `choice`/`score`/`noul`) that `lob spec-chain` already drives. Every
//! question is graded against a verified answer key (cross-checked against
//! R.G. Keen's canonical "Technology of the Fuzz Face", a real Aion
//! Electronics clone build doc, and pedalkernel's own SPICE-oracle-validated
//! `fuzz_face_pnp` fixture — not fabricated from memory).
//!
//! Several distractor options are deliberately drawn from OTHER classic
//! circuits (Tube Screamer diode-feedback clipping, a divider-biased/
//! cap-coupled generic fuzz chain like this crate's own [`topology`] module
//! produces) — a wrong-but-plausible answer here is the interesting failure
//! mode, not a random one.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example fuzzface_quiz_probe
//! ```
//! Needs `OODA_API_KEY` in the real environment, a repo-local `.env`, or
//! `~/.lob/credentials` (same precedence `lob` itself uses).

use ooda::{Client, Criteria, Question, Request};

struct QuizItem {
    name: &'static str,
    instructions: &'static str,
    options: &'static [(&'static str, &'static str)],
    correct: &'static str,
}

const QUIZ: &[QuizItem] = &[
    QuizItem {
        name: "transistor_polarity",
        instructions: "In an original, unmodified 1966-67 Dallas Arbiter Fuzz Face (the classic 2-knob Fuzz/Volume version), what type of transistors does it use?",
        options: &[
            ("pnp", "PNP germanium transistors"),
            ("npn", "NPN silicon transistors"),
        ],
        correct: "pnp",
    },
    QuizItem {
        name: "supply_convention",
        instructions: "How is the original germanium PNP Fuzz Face's 9V battery wired relative to circuit ground?",
        options: &[
            ("neg_ground_plus9", "negative ground: circuit ground = battery's negative terminal, +9V supply rail"),
            ("pos_ground_minus9", "positive ground: circuit ground = battery's positive terminal, -9V supply rail"),
        ],
        correct: "pos_ground_minus9",
    },
    QuizItem {
        name: "interstage_coupling",
        instructions: "In the classic Fuzz Face, how is the first transistor's output connected to the second transistor's input?",
        options: &[
            ("capacitor_coupled", "a coupling capacitor sits between Q1's collector and Q2's base"),
            ("direct_coupled", "Q1's collector connects straight to Q2's base with no capacitor in between"),
        ],
        correct: "direct_coupled",
    },
    QuizItem {
        name: "q1_emitter",
        instructions: "How is Q1's emitter wired in the classic Fuzz Face?",
        options: &[
            ("grounded_direct", "wired straight to ground, no resistor at all"),
            ("resistor_unbypassed", "a resistor to ground, left unbypassed for feedback"),
            ("resistor_bypassed", "a resistor to ground, bypassed by a capacitor"),
        ],
        correct: "grounded_direct",
    },
    QuizItem {
        name: "feedback_path",
        instructions: "The Fuzz Face has one resistor that forms a DC bias feedback loop between the two transistors. Which two terminals does it connect?",
        options: &[
            ("q2c_to_q1b", "from Q2's collector back to Q1's base"),
            ("q2e_to_q1b", "from Q2's emitter back to Q1's base"),
            ("q2e_to_q1e", "from Q2's emitter back to Q1's emitter"),
            ("q1c_to_q2e", "from Q1's collector forward to Q2's emitter (not feedback)"),
        ],
        correct: "q2e_to_q1b",
    },
    QuizItem {
        name: "feedback_resistor_value",
        instructions: "What is the value of that DC feedback resistor (between Q2's emitter and Q1's base)?",
        options: &[("10k", "10k"), ("33k", "33k"), ("100k", "100k"), ("220k", "220k")],
        correct: "100k",
    },
    QuizItem {
        name: "q1_collector_resistor",
        instructions: "What is the value of Q1's collector resistor?",
        options: &[("10k", "10k"), ("33k", "33k"), ("68k", "68k"), ("100k", "100k")],
        correct: "33k",
    },
    QuizItem {
        name: "fuzz_control_location",
        instructions: "The 'Fuzz' control (the gain/intensity pot) sits in which transistor's which terminal path?",
        options: &[
            ("q1_emitter", "Q1's emitter path"),
            ("q2_emitter", "Q2's emitter path"),
            ("q1_collector", "Q1's collector path"),
            ("q2_collector", "Q2's collector path"),
        ],
        correct: "q2_emitter",
    },
    QuizItem {
        name: "fuzz_pot_value",
        instructions: "What is the value of the Fuzz pot?",
        options: &[("500r", "500 ohm"), ("1k", "1k"), ("10k", "10k"), ("100k", "100k")],
        correct: "1k",
    },
    QuizItem {
        name: "volume_pot_value",
        instructions: "What is the value of the Volume pot?",
        options: &[("100k", "100k"), ("250k", "250k"), ("500k", "500k"), ("1m", "1M")],
        correct: "500k",
    },
    QuizItem {
        name: "q1_bias_headroom",
        instructions: "A healthy, correctly biased Fuzz Face has an unusual DC bias point on Q1's collector relative to ground. Which best describes it?",
        options: &[
            ("near_0v", "Q1's collector sits only a fraction of a volt above ground"),
            ("near_half_rail", "Q1's collector sits at roughly half the supply rail"),
            ("near_full_rail", "Q1's collector sits almost at the full supply rail voltage"),
            ("irrelevant_ac_coupled", "irrelevant — the input is AC-coupled so DC bias doesn't matter"),
        ],
        correct: "near_0v",
    },
    QuizItem {
        name: "clipping_mechanism",
        instructions: "What actually produces the Fuzz Face's distortion/clipping?",
        options: &[
            ("diode_clipping", "hard diode clipping in a feedback loop, like a Tube Screamer"),
            ("transistor_saturation_cutoff", "asymmetric transistor saturation/cutoff from the two gain stages themselves, no diodes involved"),
            ("opamp_rail_clipping", "op-amp output rail clipping"),
            ("digital_waveshaping", "digital waveshaping / lookup-table distortion"),
        ],
        correct: "transistor_saturation_cutoff",
    },
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ooda::CapturingClient::new(
        ooda::HttpClient::from_env()
            .map_err(|e| format!("{e} (need OODA_API_KEY — see .env.example)"))?,
        ooda::Capture::for_current_binary()?,
    );

    let observation = serde_json::json!({
        "topic": "the classic Dallas Arbiter Fuzz Face guitar fuzz pedal — the original, unmodified 1966-67 two-knob (Fuzz, Volume) germanium PNP version",
        "task": "Answer each question from real, well-established engineering knowledge of this specific, extremely well-documented vintage circuit. These are not opinions — pick the option that matches the real, original schematic.",
    });

    let mut request = Request::new(observation);
    for item in QUIZ {
        let criteria: Criteria = item.options.iter().copied().collect();
        request = request.with(item.name, Question::choice(item.instructions, criteria));
    }

    println!(
        "fuzzface_quiz_probe: {} questions, one batched decide() call\n",
        QUIZ.len()
    );
    let outcome = client.decide(&request)?;

    let mut correct_count = 0usize;
    let mut confidently_wrong = Vec::new();
    for item in QUIZ {
        let answer = outcome.answer(item.name)?;
        let picked = answer.choice().unwrap_or("<no choice>");
        let confidence = answer.confidence().unwrap_or(f64::NAN);
        let is_correct = picked == item.correct;
        if is_correct {
            correct_count += 1;
        } else if confidence > 0.7 {
            confidently_wrong.push(item.name);
        }
        println!(
            "  [{}] {:<24} picked={:<24} correct={:<24} confidence={:.2}",
            if is_correct { "PASS" } else { "FAIL" },
            item.name,
            picked,
            item.correct,
            confidence
        );
    }

    println!("\n  score: {}/{}", correct_count, QUIZ.len());
    if !confidently_wrong.is_empty() {
        println!(
            "  confidently wrong (>0.7 confidence, still incorrect): {}",
            confidently_wrong.join(", ")
        );
    }
    if let Some(model) = &outcome.resolved_model {
        println!("  resolved model: {model}");
    }

    Ok(())
}
