//! `stm32-codec` — a small STM32H7 audio board: the first curated family that
//! is a digital board rather than an audio-band analog circuit (legion-of-bom-y17.8).
//!
//! One decision is made from the brief — which of three codec options to fit —
//! and everything else is either derived or cited:
//!
//! * **Pins** are connected by the names the official KiCad symbols give them
//!   (`u1.n["VDD"]`), never by numbers typed here, and every name — plus every
//!   pin-mux alternate a role relies on (`PE5` as `SAI1_SCK_A`) — is checked
//!   against the symbol file by [`check_symbols`]. KiCad's STM32 symbols are
//!   generated from ST's own pin data, so that is the cited source for the mux.
//! * **Values** — every capacitor, resistor and strap — carry an [`Evidence`]:
//!   a verbatim quote from a pinned datasheet page that [`crate::datasheet::verify`]
//!   checks mechanically, or a reading (a figure with no text layer, or a fact
//!   the pinned sources do not state) that counts only once a person confirms it
//!   ([`unconfirmed_facts`]).
//! * **Counts** are derived: the H7 gets one 100 nF per VDD pin because its
//!   datasheet says "N x 100 nF, N = number of VDD pins", and N is read from the
//!   symbol, not written here.
//!
//! TI's PCM5102A datasheet sets µ in a symbol font that `pdftotext` renders as a
//! plain "m": the quote "0.1mF 10mF" on its p.26 is 0.1 µF and 10 µF. The quote
//! is kept verbatim (that is what makes it checkable); the reading is in `what`.

use std::collections::BTreeMap;
use std::path::Path;

use ooda::{Client, Question, Request, Trace};
use serde::{Deserialize, Serialize};

use crate::datasheet::{Citation, Datasheet, Evidence};
use crate::oscillator::{fmt_pf, load_cap_pf, round_e12_pf, CrystalOption, CRYSTAL_CATALOG};
use crate::spec::{expect_choice, SpecError};
use crate::stage::StageError;
use crate::symbols;

// ---- pinned sources ---------------------------------------------------------
//
// Every datasheet is the distributor's (LCSC's) copy, pinned by hash.

pub static STM32H743_DS: Datasheet = Datasheet {
    part: "STM32H743VIT6",
    url: "https://datasheet.lcsc.com/datasheet/pdf/880ee2974c46763ce43da1468b8b0b74.pdf",
    sha256: "9c74d2015445f7c3c2c9076fc13a2665c8f6fecec7898d128dac836d6d39314b",
};
pub static PCM5102A_DS: Datasheet = Datasheet {
    part: "PCM5102APWR",
    url: "https://datasheet.lcsc.com/datasheet/pdf/427cb50cd71d4fa092a8afe7bc909457.pdf",
    sha256: "82970ecf973a6c13a4c729c43889501d4b5b4258614a9941a5660d3c355b3c3b",
};
pub static PCM1808_DS: Datasheet = Datasheet {
    part: "PCM1808PWR",
    url: "https://datasheet.lcsc.com/datasheet/pdf/123fc00b1e2e42d784540e014c38fb88.pdf",
    sha256: "a36e2bcd3b168e3efe4c3fc8dd3d0799e2f9fc84f99aca1fd24b82a9d2cf13db",
};
pub static WM8731_DS: Datasheet = Datasheet {
    part: "WM8731SEDS",
    url: "https://datasheet.lcsc.com/datasheet/pdf/d8923c62c1264cd78eaae0b5889c3563.pdf",
    sha256: "2862107246dc672cad4358a32c0b8b8e24954ba3eefad568971c44419f7ad8fa",
};
pub static ES8388_DS: Datasheet = Datasheet {
    part: "ES8388",
    url: "https://datasheet.lcsc.com/datasheet/pdf/b5b69a0d4875954a40e8f8a3f42483af.pdf",
    sha256: "db9300f79df430d022b754b29eb1b6b17ca08e3b91d3ba833606dadc65898258",
};
pub static AMS1117_DS: Datasheet = Datasheet {
    part: "AMS1117-3.3",
    url: "https://datasheet.lcsc.com/datasheet/pdf/e6935943fc6b1bbf350a1a0f3e90dc4a.pdf",
    sha256: "189a2651878a87d590b768eaa9b44217a3fdf460352ce6ecaff127221282a3f0",
};

const fn quote(source: &'static Datasheet, page: usize, quote: &'static str) -> Evidence {
    Evidence::Quote(Citation {
        source,
        page,
        quote,
    })
}

const fn figure(source: &'static Datasheet, page: usize, what: &'static str) -> Evidence {
    Evidence::Reading {
        source: Some(source),
        page,
        what,
        confirmed_by: None,
    }
}

const fn unsourced(what: &'static str) -> Evidence {
    Evidence::Reading {
        source: None,
        page: 0,
        what,
        confirmed_by: None,
    }
}

// ---- the decision -------------------------------------------------------------

/// Which codec the board carries — the one decision the brief makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Codec {
    /// PCM5102A DAC + PCM1808 ADC: no control bus, pin-strapped, needs 5 V
    /// for the ADC's analog side.
    Pcm5102aPcm1808,
    /// WM8731: one I2C-configured codec with line and mic inputs.
    Wm8731,
    /// ES8388: one I2C-configured codec, low power, headphone-capable.
    Es8388,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::Pcm5102aPcm1808, Codec::Wm8731, Codec::Es8388];

    pub fn key(self) -> &'static str {
        match self {
            Codec::Pcm5102aPcm1808 => "pcm5102a-pcm1808",
            Codec::Wm8731 => "wm8731",
            Codec::Es8388 => "es8388",
        }
    }

    /// What a decider is told about the option — capabilities, not a verdict.
    fn describe(self) -> &'static str {
        match self {
            Codec::Pcm5102aPcm1808 => {
                "separate DAC + ADC chips, configured by pins only (no I2C), \
                 line level in and out, needs a 5 V rail"
            }
            Codec::Wm8731 => {
                "single codec configured over I2C, line inputs plus a microphone \
                 input, headphone driver, 3.3 V only"
            }
            Codec::Es8388 => {
                "single low-power codec configured over I2C, two stereo inputs, \
                 two stereo outputs including headphone, 3.3 V only"
            }
        }
    }
}

/// A decided `stm32-codec` board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stm32CodecSpec {
    pub codec: Codec,
}

/// Decide the codec from the brief.
pub fn generate_stm32_codec_spec(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
) -> Result<Stm32CodecSpec, SpecError> {
    let criteria: ooda::Criteria = Codec::ALL
        .iter()
        .map(|c| (c.key(), c.describe().to_string()))
        .collect();
    let observation =
        serde_json::json!({ "brief": brief, "task": "STM32H7 audio board codec choice" });
    let request = Request::new(observation).with(
        "codec",
        Question::choice("Which audio codec option best fits this brief?", criteria),
    );
    let outcome = client.decide(&request)?;
    let key = expect_choice(&outcome, trace, "codec")?;
    let codec = Codec::ALL
        .into_iter()
        .find(|c| c.key() == key)
        .ok_or_else(|| SpecError::Unexpected(format!("codec choice '{key}' is not an option")))?;
    Ok(Stm32CodecSpec { codec })
}

// ---- the board as parts and nets ---------------------------------------------

/// Where a part's pins come from.
#[derive(Debug, Clone, PartialEq)]
enum Symbol {
    /// An official KiCad symbol: `(library, symbol)`.
    Kicad(&'static str, &'static str),
    /// No KiCad symbol exists: pins `(number, name, I/O)` from the datasheet's
    /// own pin table, each row cited. I/O is the table's column: `I`, `O`,
    /// `I/O`, `–` (a supply or ground), or `P` for a passive node.
    Inline(&'static [(u32, &'static str, &'static str, Evidence)]),
}

#[derive(Debug, Clone, PartialEq)]
struct BoardPart {
    reference: String,
    symbol: Symbol,
    value: String,
    footprint: &'static str,
    mpn: Option<&'static str>,
    lcsc: Option<&'static str>,
    /// Why this part, at this value, is on the board.
    why: Evidence,
    /// Not an analog circuit SPICE models (an IC, a crystal): written as
    /// `Sim.Enable = 0`.
    sim_excluded: bool,
}

/// A connection: a part's pin by **name** (ICs) or by **number** (two-pin
/// passives, whose KiCad symbols number them 1 and 2).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Pin {
    Name(String),
    /// The `k`th pin named `name` — for a name the symbol repeats but the
    /// design must not join (the H7's two VCAP pins, each its own cap).
    NameAt(String, usize),
    Num(u32),
}

/// The whole board before it is written as SKiDL.
#[derive(Debug, Default)]
struct Board {
    parts: Vec<BoardPart>,
    nets: BTreeMap<String, Vec<(String, Pin)>>,
    /// `(reference, pin name, alternate)` — pin-mux roles to check against the
    /// symbol's alternates.
    mux: Vec<(String, &'static str, &'static str)>,
    /// Designator counters per prefix.
    next: BTreeMap<&'static str, usize>,
    /// Design decisions that belong to no one part (a component left out).
    notes: Vec<Evidence>,
}

const C_0603: &str = "Capacitor_SMD:C_0603_1608Metric";
const C_0805: &str = "Capacitor_SMD:C_0805_2012Metric";
const C_TANT: &str = "Capacitor_Tantalum_SMD:CP_EIA-3528-21_Kemet-B";
const R_0603: &str = "Resistor_SMD:R_0603_1608Metric";
const TEST_PAD: &str = crate::carrier::DEFAULT_TESTPOINT_FOOTPRINT;

impl Board {
    fn designator(&mut self, prefix: &'static str) -> String {
        let n = self.next.entry(prefix).or_insert(0);
        *n += 1;
        format!("{prefix}{n}")
    }

    fn connect(&mut self, net: &str, reference: &str, pin: Pin) {
        self.nets
            .entry(net.to_string())
            .or_default()
            .push((reference.to_string(), pin));
    }

    /// Connect every pin of `reference` named `name` (all five VDD pins).
    fn pin(&mut self, net: &str, reference: &str, name: &str) {
        self.connect(net, reference, Pin::Name(name.to_string()));
    }

    #[allow(clippy::too_many_arguments)]
    fn ic(
        &mut self,
        prefix: &'static str,
        symbol: Symbol,
        value: &str,
        footprint: &'static str,
        mpn: &'static str,
        lcsc: &'static str,
        why: Evidence,
    ) -> String {
        let reference = self.designator(prefix);
        self.parts.push(BoardPart {
            reference: reference.clone(),
            symbol,
            value: value.to_string(),
            footprint,
            mpn: Some(mpn),
            lcsc: Some(lcsc),
            why,
            sim_excluded: true,
        });
        reference
    }

    /// A two-terminal passive from pin 1 on `a` to pin 2 on `b`.
    fn passive(
        &mut self,
        kind: &'static str,
        value: &str,
        footprint: &'static str,
        a: &str,
        b: &str,
        why: Evidence,
    ) -> String {
        let (prefix, symbol) = match kind {
            "R" => ("R", Symbol::Kicad("Device", "R")),
            "CP" => ("C", Symbol::Kicad("Device", "C_Polarized")),
            _ => ("C", Symbol::Kicad("Device", "C")),
        };
        let reference = self.designator(prefix);
        self.parts.push(BoardPart {
            reference: reference.clone(),
            symbol,
            value: value.to_string(),
            footprint,
            mpn: None,
            lcsc: None,
            why,
            sim_excluded: false,
        });
        self.connect(a, &reference, Pin::Num(1));
        self.connect(b, &reference, Pin::Num(2));
        reference
    }

    /// The datasheet's "0.1 µF + 10 µF" on a supply pin, both to ground.
    fn bypass_pair(&mut self, rail: &str, why: Evidence) {
        self.passive("C", "100nF", C_0603, rail, "GND", why);
        self.passive("C", "10uF", C_0805, rail, "GND", why);
    }

    /// A test pad (a pogo-pin landing, footprint only — no part is fitted).
    fn test_pad(&mut self, net: &str, why: Evidence) {
        let reference = self.designator("TP");
        self.parts.push(BoardPart {
            reference: reference.clone(),
            symbol: Symbol::Kicad("Connector", "TestPoint"),
            value: net.to_string(),
            footprint: TEST_PAD,
            mpn: None,
            lcsc: None,
            why,
            sim_excluded: false,
        });
        self.connect(net, &reference, Pin::Num(1));
    }

    /// Every piece of evidence the board rests on.
    fn evidence(&self) -> Vec<Evidence> {
        let mut out: Vec<Evidence> = self.parts.iter().map(|p| p.why).collect();
        out.extend(self.notes.iter().copied());
        for p in &self.parts {
            if let Symbol::Inline(pins) = p.symbol {
                out.extend(pins.iter().map(|(_, _, _, e)| *e));
            }
        }
        out
    }
}

// ---- the H7 core ---------------------------------------------------------------

const H7: Symbol = Symbol::Kicad("MCU_ST_STM32H7", "STM32H743VITx");
/// VDD pins on the H743VIT's LQFP-100 — read from the symbol by
/// [`check_symbols`], which fails if this disagrees.
const H7_VDD_PINS: usize = 5;

/// The SAI1 pins the codecs hang off, as `(port pin, alternate function)`.
/// Block A drives the clocks and the DAC data; block B, synchronous to it,
/// receives the ADC data.
const SAI_MCLK: (&str, &str) = ("PE2", "SAI1_MCLK_A");
const SAI_SD_B: (&str, &str) = ("PE3", "SAI1_SD_B");
const SAI_FS: (&str, &str) = ("PE4", "SAI1_FS_A");
const SAI_SCK: (&str, &str) = ("PE5", "SAI1_SCK_A");
const SAI_SD_A: (&str, &str) = ("PE6", "SAI1_SD_A");
const I2C_SCL: (&str, &str) = ("PB6", "I2C1_SCL");
const I2C_SDA: (&str, &str) = ("PB7", "I2C1_SDA");
const SWDIO: (&str, &str) = ("PA13", "DEBUG_JTMS-SWDIO");
const SWCLK: (&str, &str) = ("PA14", "DEBUG_JTCK-SWCLK");

/// The crystal for the H7's HSE: the lowest-frequency MHz part in the sourced
/// crystal catalog inside the HSE's 4–48 MHz range (datasheet p.128).
fn hse_crystal() -> &'static CrystalOption {
    CRYSTAL_CATALOG
        .iter()
        .filter(|c| (4e6..=48e6).contains(&c.hz))
        .min_by(|a, b| a.hz.total_cmp(&b.hz))
        .expect("the crystal catalog has a MHz-range part")
}

fn h7_core(b: &mut Board) -> String {
    let u = b.ic(
        "U",
        H7,
        "STM32H743VIT6",
        "Package_QFP:LQFP-100_14x14mm_P0.5mm",
        "STM32H743VIT6",
        "C114409",
        quote(&STM32H743_DS, 27, "Boot modes"),
    );
    for (net, name) in [
        ("+3V3", "VDD"),
        ("+3V3", "VDDA"),
        ("+3V3", "VREF+"),
        ("+3V3", "VBAT"),
        ("GND", "VSS"),
        ("GND", "VSSA"),
        ("NRST", "NRST"),
        ("BOOT0", "BOOT0"),
    ] {
        b.pin(net, &u, name);
    }
    let fig15 = |q| quote(&STM32H743_DS, 105, q);
    for _ in 0..H7_VDD_PINS {
        b.passive("C", "100nF", C_0603, "+3V3", "GND", fig15("N(1) x 100 nF"));
    }
    b.passive("C", "4.7uF", C_0805, "+3V3", "GND", fig15("+ 1 x 4.7 μF"));
    // Each VCAP pin is a regulator output with its own cap: tying the two
    // together is output-to-output, which ERC rightly rejects.
    for k in 0..2 {
        let net = format!("VCAP{}", k + 1);
        b.connect(&net, &u, Pin::NameAt("VCAP".into(), k));
        b.passive("C", "2.2uF", C_0603, &net, "GND", fig15("2 x 2.2μF"));
    }
    // VDDA and VREF+ share the 3.3 V rail; each keeps its own pair.
    for _ in 0..2 {
        b.passive(
            "C",
            "100nF",
            C_0603,
            "+3V3",
            "GND",
            fig15("100 nF + 1 x 1 μF"),
        );
        b.passive(
            "C",
            "1uF",
            C_0603,
            "+3V3",
            "GND",
            fig15("100 nF + 1 x 1 μF"),
        );
    }
    b.passive(
        "C",
        "100nF",
        C_0603,
        "NRST",
        "GND",
        quote(&STM32H743_DS, 146, "Recommended NRST pin protection"),
    );
    b.passive(
        "R",
        "10k",
        R_0603,
        "BOOT0",
        "GND",
        unsourced(
            "BOOT0 held low through 10k so the part boots from flash (AN2606 / RM0433, not pinned)",
        ),
    );

    // HSE crystal on PH0/PH1, load caps computed from the crystal's own CL.
    let xtal = hse_crystal();
    let y = b.designator("Y");
    b.parts.push(BoardPart {
        reference: y.clone(),
        symbol: Symbol::Kicad("Device", "Crystal"),
        value: format!("{:.0}MHz", xtal.hz / 1e6),
        footprint: xtal.footprint,
        mpn: Some(xtal.mpn),
        lcsc: None,
        why: quote(&STM32H743_DS, 128, "4 to 48 MHz crystal/ceramic"),
        sim_excluded: true,
    });
    b.pin("HSE_IN", &u, "PH0");
    b.pin("HSE_OUT", &u, "PH1");
    b.connect("HSE_IN", &y, Pin::Num(1));
    b.connect("HSE_OUT", &y, Pin::Num(2));
    let cl = fmt_pf(round_e12_pf(load_cap_pf(xtal.cl_pf)));
    for net in ["HSE_IN", "HSE_OUT"] {
        b.passive(
            "C",
            &cl,
            C_0603,
            net,
            "GND",
            quote(
                &STM32H743_DS,
                128,
                "the resonator and the load capacitors have to be placed as close",
            ),
        );
    }

    // SWD on pogo pads: footprint only, flashed in-house.
    for (net, (pin, alt)) in [("SWDIO", SWDIO), ("SWCLK", SWCLK)] {
        b.pin(net, &u, pin);
        b.mux.push((u.clone(), pin, alt));
    }
    for net in ["SWDIO", "SWCLK", "NRST", "+3V3", "GND"] {
        b.test_pad(
            net,
            unsourced("SWD pogo-pin pads, footprint only (house practice: flashed in-house)"),
        );
    }
    for (net, (pin, alt)) in [
        ("SAI_MCLK", SAI_MCLK),
        ("SAI_SCK", SAI_SCK),
        ("SAI_FS", SAI_FS),
        ("SAI_SD_A", SAI_SD_A),
        ("SAI_SD_B", SAI_SD_B),
    ] {
        b.pin(net, &u, pin);
        b.mux.push((u.clone(), pin, alt));
    }
    u
}

/// 5 V in on a header, 3.3 V out of an AMS1117.
fn power(b: &mut Board) {
    let j = b.designator("J");
    b.parts.push(BoardPart {
        reference: j.clone(),
        symbol: Symbol::Kicad("Connector_Generic", "Conn_01x02"),
        value: "5V in".into(),
        footprint: "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
        mpn: None,
        lcsc: None,
        why: unsourced("5 V power entry on a 2-pin header"),
        sim_excluded: false,
    });
    b.connect("+5V", &j, Pin::Num(1));
    b.connect("GND", &j, Pin::Num(2));
    let u = b.ic(
        "U",
        Symbol::Kicad("Regulator_Linear", "AMS1117-3.3"),
        "AMS1117-3.3",
        "Package_TO_SOT_SMD:SOT-223-3_TabPin2",
        "AMS1117-3.3",
        "C6186",
        quote(&AMS1117_DS, 1, "AMS1117"),
    );
    b.pin("+5V", &u, "VI");
    b.pin("+3V3", &u, "VO");
    b.pin("GND", &u, "GND");
    b.passive(
        "CP",
        "22uF",
        C_TANT,
        "+3V3",
        "GND",
        quote(
            &AMS1117_DS,
            4,
            "The addition of 22µF solid tantalum on the output will ensure",
        ),
    );
    b.passive(
        "C",
        "10uF",
        C_0805,
        "+5V",
        "GND",
        figure(
            &AMS1117_DS,
            4,
            "10µF on the input in the application figure",
        ),
    );
}

/// The audio I/O header: line out L/R, line in L/R, grounds.
fn audio_header(b: &mut Board) {
    header(
        b,
        "Audio I/O",
        &["OUT_L", "OUT_R", "GND", "IN_L", "IN_R", "GND"],
        unsourced("line-level audio I/O on a header (jacks are a later option)"),
    );
}

/// A 1xN 2.54mm pin header carrying `nets` in pin order.
fn header(b: &mut Board, value: &str, nets: &[&str], why: Evidence) {
    const HEADERS: [(&str, &str); 6] = [
        (
            "Conn_01x01",
            "Connector_PinHeader_2.54mm:PinHeader_1x01_P2.54mm_Vertical",
        ),
        (
            "Conn_01x02",
            "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
        ),
        (
            "Conn_01x03",
            "Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical",
        ),
        (
            "Conn_01x04",
            "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical",
        ),
        (
            "Conn_01x05",
            "Connector_PinHeader_2.54mm:PinHeader_1x05_P2.54mm_Vertical",
        ),
        (
            "Conn_01x06",
            "Connector_PinHeader_2.54mm:PinHeader_1x06_P2.54mm_Vertical",
        ),
    ];
    let (symbol, footprint) = HEADERS[nets.len() - 1];
    let j = b.designator("J");
    b.parts.push(BoardPart {
        reference: j.clone(),
        symbol: Symbol::Kicad("Connector_Generic", symbol),
        value: value.into(),
        footprint,
        mpn: None,
        lcsc: None,
        why,
        sim_excluded: false,
    });
    for (n, net) in nets.iter().enumerate() {
        b.connect(net, &j, Pin::Num(n as u32 + 1));
    }
}

/// Shared I2C for the codecs that have a control bus.
fn i2c(b: &mut Board, mcu: &str) {
    for (net, (pin, alt)) in [("I2C_SCL", I2C_SCL), ("I2C_SDA", I2C_SDA)] {
        b.pin(net, mcu, pin);
        b.mux.push((mcu.to_string(), pin, alt));
        b.passive(
            "R",
            "10k",
            R_0603,
            net,
            "+3V3",
            figure(
                &ES8388_DS,
                7,
                "10K pull-ups on the I2C control lines in the typical application circuit",
            ),
        );
    }
}

// ---- codecs -------------------------------------------------------------------

/// PCM1808 pin table (datasheet p.5) — it has no KiCad symbol.
static PCM1808_PINS: [(u32, &str, &str, Evidence); 14] = [
    (1, "VREF", "P", quote(&PCM1808_DS, 5, "VREF 1 14 VINR")),
    (2, "AGND", "–", quote(&PCM1808_DS, 5, "AGND 2 – Analog GND")),
    (
        3,
        "VCC",
        "–",
        quote(&PCM1808_DS, 5, "VCC 3 – Analog power supply, 5-V"),
    ),
    (
        4,
        "VDD",
        "–",
        quote(&PCM1808_DS, 5, "VDD 4 – Digital power supply, 3.3-V"),
    ),
    (
        5,
        "DGND",
        "–",
        quote(&PCM1808_DS, 5, "DGND 5 – Digital GND"),
    ),
    (
        6,
        "SCKI",
        "I",
        quote(&PCM1808_DS, 5, "SCKI 6 I System clock input"),
    ),
    (
        7,
        "LRCK",
        "I/O",
        quote(
            &PCM1808_DS,
            5,
            "LRCK 7 I/O Audio data latch enable input/output",
        ),
    ),
    (
        8,
        "BCK",
        "I/O",
        quote(
            &PCM1808_DS,
            5,
            "BCK 8 I/O Audio data bit clock input/output",
        ),
    ),
    (
        9,
        "DOUT",
        "O",
        quote(&PCM1808_DS, 5, "DOUT 9 O Audio data digital output"),
    ),
    (
        10,
        "MD0",
        "I",
        quote(&PCM1808_DS, 5, "MD0 10 I Audio interface mode select 0"),
    ),
    (
        11,
        "MD1",
        "I",
        quote(&PCM1808_DS, 5, "MD1 11 I Audio interface mode select 1"),
    ),
    (
        12,
        "FMT",
        "I",
        quote(&PCM1808_DS, 5, "FMT 12 I Audio interface format select"),
    ),
    (
        13,
        "VINL",
        "I",
        quote(&PCM1808_DS, 5, "VINL 13 I Analog input, L-channel"),
    ),
    (
        14,
        "VINR",
        "I",
        quote(&PCM1808_DS, 5, "VINR 14 I Analog input, R-channel"),
    ),
];

fn pcm_pair(b: &mut Board) {
    // PCM5102A — the DAC.
    let dac = b.ic(
        "U",
        Symbol::Kicad("Audio", "PCM5102A"),
        "PCM5102A",
        "Package_SO:TSSOP-20_4.4x6.5mm_P0.65mm",
        "PCM5102APWR",
        "C107671",
        quote(&PCM5102A_DS, 26, "3-wire I2S interface (BCK PLL)"),
    );
    let p26 = |q| quote(&PCM5102A_DS, 26, q);
    for (rail, pin, why) in [
        (
            "+3V3",
            "AVDD",
            p26("BCK            AVDD           0.1mF       10mF"),
        ),
        ("+3V3", "CPVDD", p26("CAPP           0.1mF       10mF")),
        ("+3V3", "DVDD", p26("10mF                        0.1mF")),
    ] {
        b.pin(rail, &dac, pin);
        b.bypass_pair(rail, why);
    }
    for gnd in ["AGND", "DGND", "CPGND"] {
        b.pin("GND", &dac, gnd);
    }
    b.pin("DAC_CAPP", &dac, "CAPP");
    b.pin("DAC_CAPM", &dac, "CAPM");
    b.pin("DAC_VNEG", &dac, "VNEG");
    b.pin("DAC_LDOO", &dac, "LDOO");
    b.passive(
        "C",
        "2.2uF",
        C_0603,
        "DAC_CAPP",
        "DAC_CAPM",
        p26("CAPM   2.2mF   2.2mF"),
    );
    b.passive(
        "C",
        "2.2uF",
        C_0603,
        "DAC_VNEG",
        "GND",
        p26("CAPM   2.2mF   2.2mF"),
    );
    b.passive(
        "C",
        "100nF",
        C_0603,
        "DAC_LDOO",
        "GND",
        quote(
            &PCM5102A_DS,
            28,
            "Should be used with a 0.1-µF decoupling cap.",
        ),
    );
    // Straps: I2S, normal-latency filter, no de-emphasis, unmuted, BCK PLL.
    for pin in ["FMT", "FLT", "DEMP", "SCK"] {
        b.pin("GND", &dac, pin);
    }
    b.pin("+3V3", &dac, "XSMT");
    b.pin("SAI_SCK", &dac, "BCK");
    b.pin("SAI_FS", &dac, "LRCK");
    b.pin("SAI_SD_A", &dac, "DIN");
    let filter = quote(
        &PCM5102A_DS,
        8,
        "with 470-Ω output resistor and a 2.2-nF shunt capacitor",
    );
    for (pin, node, out) in [("OUTL", "DAC_OUTL", "OUT_L"), ("OUTR", "DAC_OUTR", "OUT_R")] {
        b.pin(node, &dac, pin);
        b.passive("R", "470", R_0603, node, out, filter);
        b.passive("C", "2.2nF", C_0603, out, "GND", filter);
    }

    // PCM1808 — the ADC, 5 V analog.
    let adc = b.ic(
        "U",
        Symbol::Inline(&PCM1808_PINS),
        "PCM1808",
        "Package_SO:TSSOP-14_4.4x5mm_P0.65mm",
        "PCM1808PWR",
        "C55513",
        quote(
            &PCM1808_DS,
            14,
            "Slave mode (256 fS, 384 fS, 512 fS autodetection)",
        ),
    );
    let p19 = |q| quote(&PCM1808_DS, 19, q);
    let supplies = p19("C3, C4: Bypass capacitors, 0.1-µF ceramic and 10-µF electrolytic");
    b.pin("+5V", &adc, "VCC");
    b.bypass_pair("+5V", supplies);
    b.pin("+3V3", &adc, "VDD");
    b.bypass_pair("+3V3", supplies);
    b.pin("ADC_VREF", &adc, "VREF");
    b.bypass_pair(
        "ADC_VREF",
        p19("C5: 0.1-µF ceramic and 10-µF electrolytic capacitors are recommended."),
    );
    b.pin("GND", &adc, "AGND");
    b.pin("GND", &adc, "DGND");
    for pin in ["MD0", "MD1", "FMT"] {
        b.pin("GND", &adc, pin);
    }
    b.pin("SAI_MCLK", &adc, "SCKI");
    b.pin("SAI_SCK", &adc, "BCK");
    b.pin("SAI_FS", &adc, "LRCK");
    b.pin("SAI_SD_B", &adc, "DOUT");
    let coupling = p19("C1, C2: A 1-µF electrolytic capacitor gives 2.7 Hz");
    for (pin, node, inp) in [("VINL", "ADC_VINL", "IN_L"), ("VINR", "ADC_VINR", "IN_R")] {
        b.pin(node, &adc, pin);
        b.passive("C", "1uF", C_0603, inp, node, coupling);
    }
}

fn wm8731(b: &mut Board, mcu: &str) {
    let u = b.ic(
        "U",
        Symbol::Kicad("Audio", "WM8731SEDS"),
        "WM8731",
        "Package_SO:SSOP-28_5.3x10.2mm_P0.65mm",
        "WM8731SEDS",
        "C41511",
        quote(
            &WM8731_DS,
            41,
            "12.288MHz (256fs) or 18.432MHz (384fs) can be used.",
        ),
    );
    let fig57 = figure(
        &WM8731_DS,
        60,
        "Figure 57: 10µF + 0.1µF on each of DBVDD, DCVDD, AVDD and HPVDD",
    );
    for pin in ["DBVDD", "DCVDD", "AVDD", "HPVDD"] {
        b.pin("+3V3", &u, pin);
        b.bypass_pair("+3V3", fig57);
    }
    for gnd in ["DGND", "AGND", "HPGND"] {
        b.pin("GND", &u, gnd);
    }
    b.pin("WM_VMID", &u, "VMID");
    b.bypass_pair(
        "WM_VMID",
        quote(
            &WM8731_DS,
            9,
            "VMID decoupled with 10uF and 0.1uF capacitors",
        ),
    );
    // Control: 2-wire (MODE low), address 0011010 (CSB low).
    b.pin("GND", &u, "MODE");
    b.pin("GND", &u, "~{CSB}");
    i2c(b, mcu);
    b.pin("I2C_SCL", &u, "SCLK");
    b.pin("I2C_SDA", &u, "SDIN");
    b.pin("SAI_MCLK", &u, "XTI/MCLK");
    b.pin("SAI_SCK", &u, "BCLK");
    b.pin("SAI_FS", &u, "DACLRC");
    b.pin("SAI_FS", &u, "ADCLRC");
    b.pin("SAI_SD_A", &u, "DACDAT");
    b.pin("SAI_SD_B", &u, "ADCDAT");
    let line_in = quote(&WM8731_DS, 23, "R1 = 5.6k, R2 = 5.6k, C1 = 220pF, C2 = 1");
    for (pin, inp) in [("LLINEIN", "IN_L"), ("RLINEIN", "IN_R")] {
        let (mid, pad) = (format!("WM_{pin}_R"), format!("WM_{pin}"));
        b.passive("R", "5.6k", R_0603, inp, &mid, line_in);
        b.passive("R", "5.6k", R_0603, &mid, "GND", line_in);
        b.passive("C", "220pF", C_0603, &mid, "GND", line_in);
        b.passive("C", "1uF", C_0603, &mid, &pad, line_in);
        b.pin(&pad, &u, pin);
    }
    // Line out: the section text recommends C1 = 10 µF; Figure 57 draws 1 µF.
    // The section-specific text wins, and only lowers the high-pass corner.
    let line_out = quote(&WM8731_DS, 29, "Recommended values are C1 = 10");
    for (pin, out) in [("LOUT", "OUT_L"), ("ROUT", "OUT_R")] {
        let (pad, mid) = (format!("WM_{pin}"), format!("WM_{pin}_C"));
        b.pin(&pad, &u, pin);
        b.passive("C", "10uF", C_0805, &pad, &mid, line_out);
        b.passive("R", "47k", R_0603, &mid, "GND", line_out);
        b.passive("R", "100", R_0603, &mid, out, line_out);
    }
    // Microphone: MICBIAS through R1 to the mic node, R2 and C1 to ground, C2
    // in series to MICIN (p.24).
    let mic = quote(
        &WM8731_DS,
        24,
        "Recommended component values are C1 = 220pF (npo ceramic), C2 = 1\u{b5}F, R1 = 680 \u{3a9}, R2 = 47k.",
    );
    b.pin("WM_MICBIAS", &u, "MICBIAS");
    b.passive("R", "680", R_0603, "WM_MICBIAS", "MIC", mic);
    b.passive("R", "47k", R_0603, "MIC", "GND", mic);
    b.passive("C", "220pF", C_0603, "MIC", "GND", mic);
    b.pin("WM_MICIN", &u, "MICIN");
    b.passive("C", "1uF", C_0603, "MIC", "WM_MICIN", mic);
    b.notes.push(figure(
        &WM8731_DS,
        60,
        "Figure 57's gain-dependent Rmic between C2 and MICIN is omitted (0 \u{3a9}): mic gain set in the codec's registers",
    ));
    header(b, "Mic in", &["MIC", "GND"], mic);
}

fn es8388(b: &mut Board, mcu: &str) {
    let u = b.ic(
        "U",
        Symbol::Kicad("Audio", "ES8388"),
        "ES8388",
        "Package_DFN_QFN:QFN-28-1EP_4x4mm_P0.45mm_EP2.6x2.6mm",
        "ES8388",
        "C365736",
        quote(&ES8388_DS, 7, "256, 384, 512, 768, 1024"),
    );
    let fig = |what| figure(&ES8388_DS, 7, what);
    for pin in ["DVDD", "PVDD", "AVDD", "HPVDD"] {
        b.pin("+3V3", &u, pin);
        b.bypass_pair(
            "+3V3",
            fig("10uF + 0.1uF on each supply pin group in the typical application circuit"),
        );
    }
    for gnd in ["DGND", "AGND", "HPGND", "EPAD"] {
        b.pin("GND", &u, gnd);
    }
    for (pin, net) in [
        ("VMID", "ES_VMID"),
        ("ADCVREF", "ES_ADCVREF"),
        ("VREF", "ES_VREF"),
    ] {
        b.pin(net, &u, pin);
        b.bypass_pair(net, fig("10uF + 0.1uF on VMID, ADCVREF and VREF"));
    }
    // I2C with CE low: address 0010000.
    b.pin("GND", &u, "CE");
    i2c(b, mcu);
    b.pin("I2C_SCL", &u, "CCLK");
    b.pin("I2C_SDA", &u, "CDATA");
    b.pin("SAI_MCLK", &u, "MCLK");
    b.pin("SAI_SCK", &u, "SCLK");
    b.pin("SAI_FS", &u, "LRCK");
    b.pin("SAI_SD_A", &u, "DSDIN");
    b.pin("SAI_SD_B", &u, "ASDOUT");
    for (pin, inp) in [("LIN1", "IN_L"), ("RIN1", "IN_R")] {
        let pad = format!("ES_{pin}");
        b.passive(
            "C",
            "1uF",
            C_0603,
            inp,
            &pad,
            fig("1uF series on each line input"),
        );
        b.pin(&pad, &u, pin);
    }
    for (pin, out) in [("LOUT1", "OUT_L"), ("ROUT1", "OUT_R")] {
        let pad = format!("ES_{pin}");
        b.pin(&pad, &u, pin);
        b.passive(
            "C",
            "1uF",
            C_0603,
            &pad,
            out,
            fig("1uF series on LOUT1/ROUT1 used as line output"),
        );
    }
}

fn board(spec: &Stm32CodecSpec) -> Board {
    let mut b = Board::default();
    let mcu = h7_core(&mut b);
    power(&mut b);
    audio_header(&mut b);
    match spec.codec {
        Codec::Pcm5102aPcm1808 => pcm_pair(&mut b),
        Codec::Wm8731 => wm8731(&mut b, &mcu),
        Codec::Es8388 => es8388(&mut b, &mcu),
    }
    b
}

// ---- checks -------------------------------------------------------------------

/// Every fact the board for `spec` rests on.
pub fn evidence(spec: &Stm32CodecSpec) -> Vec<Evidence> {
    board(spec).evidence()
}

/// The readings no person has confirmed yet — what stands between this board
/// and a fab order.
pub fn unconfirmed_facts(spec: &Stm32CodecSpec) -> Vec<String> {
    let mut out: Vec<String> = evidence(spec)
        .iter()
        .filter_map(Evidence::unconfirmed)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Check every pin name and pin-mux alternate the board uses against the KiCad
/// symbols they come from, and the derived VDD count against the H7's symbol.
/// Returns one line per problem; empty means every name exists.
pub fn check_symbols(spec: &Stm32CodecSpec, symbol_dir: &Path) -> Result<Vec<String>, StageError> {
    let b = board(spec);
    let mut problems = Vec::new();
    let mut read: BTreeMap<(&str, &str), symbols::SymbolData> = BTreeMap::new();
    for p in &b.parts {
        if let Symbol::Kicad(lib, name) = p.symbol {
            if let std::collections::btree_map::Entry::Vacant(slot) = read.entry((lib, name)) {
                slot.insert(
                    symbols::read_symbol(symbol_dir, lib, name)?.ok_or_else(|| {
                        StageError::Other(format!("no KiCad symbol {lib}:{name}"))
                    })?,
                );
            }
        }
    }
    let data_of = |reference: &str| {
        b.parts
            .iter()
            .find(|p| p.reference == reference)
            .and_then(|p| match p.symbol {
                Symbol::Kicad(lib, name) => read
                    .get(&(lib, name))
                    .map(|d| (d.pins.clone(), &d.alternates)),
                Symbol::Inline(pins) => Some((
                    pins.iter()
                        .map(|(n, name, _, _)| (n.to_string(), name.to_string()))
                        .collect(),
                    &EMPTY,
                )),
            })
    };
    for (net, pins) in &b.nets {
        for (reference, pin) in pins {
            let Some((sym_pins, _)) = data_of(reference) else {
                problems.push(format!("{net}: {reference} is not a part on the board"));
                continue;
            };
            let found = match pin {
                Pin::Name(n) => sym_pins.iter().any(|(_, name)| name == n),
                Pin::NameAt(n, k) => sym_pins.iter().filter(|(_, name)| name == n).count() > *k,
                Pin::Num(n) => sym_pins.iter().any(|(num, _)| num == &n.to_string()),
            };
            if !found {
                problems.push(format!("{net}: {reference} has no pin {pin:?}"));
            }
        }
    }
    for (reference, pin, alt) in &b.mux {
        let Some((sym_pins, alternates)) = data_of(reference) else {
            continue;
        };
        let numbers: Vec<&String> = sym_pins
            .iter()
            .filter(|(_, name)| name == pin)
            .map(|(num, _)| num)
            .collect();
        if !alternates
            .iter()
            .any(|(num, a)| numbers.contains(&num) && a == alt)
        {
            problems.push(format!("{reference}.{pin} has no alternate function {alt}"));
        }
    }
    if let Some(h7) = read.get(&("MCU_ST_STM32H7", "STM32H743VITx")) {
        let vdd = h7.pins.iter().filter(|(_, n)| n == "VDD").count();
        if vdd != H7_VDD_PINS {
            problems.push(format!(
                "STM32H743VITx has {vdd} VDD pins; the board decouples {H7_VDD_PINS}"
            ));
        }
    }
    Ok(problems)
}

static EMPTY: Vec<(String, String)> = Vec::new();

// ---- SKiDL ----------------------------------------------------------------------

/// The SKiDL circuit for `spec` — a pure function of it.
pub fn render_stm32_codec_skidl(spec: &Stm32CodecSpec) -> String {
    let b = board(spec);
    let mut body = String::new();
    for p in &b.parts {
        let var = p.reference.to_lowercase();
        match p.symbol {
            Symbol::Kicad(lib, name) => body.push_str(&format!(
                "    {var} = Part({lib:?}, {name:?}, ref={r:?}, value={v:?}, footprint={f:?})\n",
                r = p.reference,
                v = p.value,
                f = p.footprint
            )),
            Symbol::Inline(pins) => {
                let pins: Vec<String> = pins
                    .iter()
                    .map(|(n, name, io, _)| {
                        let func = match *io {
                            "I" => "INPUT",
                            "O" => "OUTPUT",
                            "I/O" => "BIDIR",
                            "–" => "PWRIN",
                            _ => "PASSIVE",
                        };
                        format!("Pin(num={n}, name={name:?}, func=Pin.types.{func})")
                    })
                    .collect();
                body.push_str(&format!(
                    "    {var} = Part(tool=SKIDL, name={v:?}, ref={r:?}, value={v:?}, footprint={f:?},\n                 pins=[{pins}])\n",
                    r = p.reference,
                    v = p.value,
                    f = p.footprint,
                    pins = pins.join(", ")
                ));
            }
        }
        if let Some(mpn) = p.mpn {
            body.push_str(&format!("    {var}.fields[\"MPN\"] = {mpn:?}\n"));
        }
        if let Some(lcsc) = p.lcsc {
            body.push_str(&format!("    {var}.fields[\"LCSC\"] = {lcsc:?}\n"));
        }
        if p.sim_excluded {
            body.push_str(&format!("    {var}.fields[\"Sim.Enable\"] = \"0\"\n"));
        }
        body.push_str(&format!("    parts.append({var})\n"));
    }
    body.push('\n');
    for (net, pins) in &b.nets {
        let refs: Vec<String> = pins
            .iter()
            .map(|(r, pin)| match pin {
                Pin::Name(n) => format!("pins({}, {n:?})", r.to_lowercase()),
                Pin::NameAt(n, k) => format!("pins({}, {n:?})[{k}]", r.to_lowercase()),
                Pin::Num(n) => format!("{}.p[{n}]", r.to_lowercase()),
            })
            .collect();
        body.push_str(&format!("    Net({net:?}).connect({})\n", refs.join(", ")));
    }

    format!(
        r#"#!/usr/bin/env python3
"""STM32H743 + {codec} audio board.

Generated by legion-of-bom (crates/core/src/mcu_audio.rs) from a decided
stm32-codec spec. Pins are connected by the names the official KiCad symbols
give them; every value on the board carries a datasheet citation in that
module. Run standalone (needs KICAD9_SYMBOL_DIR); `lob run` sets it up.
"""

import argparse
import builtins
import sys

from skidl import ERC, SKIDL, Net, Part, Pin, generate_netlist


def pins(part, name):
    """Every pin of `part` named `name` -- and a hard failure if there is none,
    where SKiDL itself would only log it."""
    found = part.n[name]
    # A plain list: SKiDL's NetPinList indexes like a bus, not a sequence.
    found = list(found) if isinstance(found, list) else [found]
    if not found or found == [None]:
        raise SystemExit(f"{{part.ref}} has no pin named {{name!r}}")
    return found


def build():
    parts = []
{body}
    # Every pin the design does not use is an explicit no-connect.
    for part in parts:
        for p in part.pins:
            if not p.is_connected():
                p += builtins.NC
    return parts


def main():
    parser = argparse.ArgumentParser(description="STM32H743 + {codec} audio board")
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
        codec = spec.codec.key(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_option_builds_a_board_whose_every_part_has_a_reason() {
        for codec in Codec::ALL {
            let b = board(&Stm32CodecSpec { codec });
            assert!(b.parts.len() > 20, "{codec:?}: {} parts", b.parts.len());
            let refs: std::collections::HashSet<&str> =
                b.parts.iter().map(|p| p.reference.as_str()).collect();
            assert_eq!(
                refs.len(),
                b.parts.len(),
                "{codec:?}: duplicate designators"
            );
            for (net, pins) in &b.nets {
                for (r, _) in pins {
                    assert!(
                        refs.contains(r.as_str()),
                        "{codec:?}: {net} names missing part {r}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_h7_gets_one_bypass_cap_per_vdd_pin_plus_the_bulk_cap() {
        let b = board(&Stm32CodecSpec {
            codec: Codec::Pcm5102aPcm1808,
        });
        let cited_n = b
            .parts
            .iter()
            .filter(|p| matches!(p.why, Evidence::Quote(c) if c.quote == "N(1) x 100 nF"))
            .count();
        assert_eq!(cited_n, H7_VDD_PINS);
    }

    #[test]
    fn a_board_is_not_fab_ready_until_its_readings_are_confirmed() {
        let unconfirmed = unconfirmed_facts(&Stm32CodecSpec {
            codec: Codec::Wm8731,
        });
        assert!(
            unconfirmed.iter().any(|u| u.contains("Figure 57")),
            "WM8731's supply decoupling is read off a figure and must need sign-off: {unconfirmed:#?}"
        );
        assert!(
            unconfirmed
                .iter()
                .any(|u| u.starts_with("unsourced: BOOT0")),
            "{unconfirmed:#?}"
        );
    }

    #[test]
    fn the_skidl_connects_ic_pins_by_name_and_passives_by_number() {
        let py = render_stm32_codec_skidl(&Stm32CodecSpec {
            codec: Codec::Es8388,
        });
        assert!(py.contains(r#"pins(u1, "VDD")"#), "{py}");
        assert!(py.contains(r#"pins(u1, "PE5")"#));
        assert!(py.contains(".p[1]"));
        assert!(py.contains(r#"fields["LCSC"] = "C365736""#));
    }

    #[test]
    fn the_codec_is_decided_from_the_brief() {
        let client = ooda::ScriptedClient::new([r#"{"answers": {"codec":
            {"type":"choice","choice":"wm8731","confidence":0.8,"probabilities":{"wm8731":0.8}}}}"#
            .to_string()]);
        let mut trace = Trace::new();
        let spec = generate_stm32_codec_spec(&client, &mut trace, "needs a mic input").unwrap();
        assert_eq!(spec.codec, Codec::Wm8731);
        assert_eq!(trace.records().len(), 1);
    }

    /// Needs the installed KiCad symbol library, so it is ignored by default
    /// rather than silently passing without it (legion-of-bom-69v.2). Run with
    /// `cargo test -p legion-of-bom-core -- --ignored mcu_audio`.
    #[test]
    #[ignore = "needs KiCad symbols"]
    fn every_pin_name_and_alternate_exists_in_the_kicad_symbols() {
        let dir = crate::skidl::kicad_symbol_dir().expect("KiCad symbol library");
        for codec in Codec::ALL {
            let problems = check_symbols(&Stm32CodecSpec { codec }, dir.path()).unwrap();
            assert!(problems.is_empty(), "{codec:?}: {problems:#?}");
        }
    }

    /// Needs the pinned datasheets (fetched into the cache on first run).
    #[test]
    #[ignore = "needs the pinned datasheets and pdftotext"]
    fn every_cited_quote_is_on_its_page() {
        let mut citations: Vec<Citation> = Vec::new();
        for codec in Codec::ALL {
            citations.extend(
                evidence(&Stm32CodecSpec { codec })
                    .iter()
                    .filter_map(|e| e.quote().copied()),
            );
        }
        citations.sort_by_key(|c| (c.source.sha256, c.page, c.quote));
        citations.dedup();
        let failures =
            crate::datasheet::verify(&citations, &crate::datasheet::default_cache_dir()).unwrap();
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
