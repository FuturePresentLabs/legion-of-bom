//! Generated footprints for **sub-boards mounted through headers** — a Daisy
//! Seed, a controls PCB in a 2-board stack, or a generic board-to-board header.
//!
//! The organising idea (epic `25z`): a sub-board is just a *part* whose footprint
//! is a header pin layout plus a body-sized courtyard (the keep-out the sub-board
//! occupies above the main board). Modelled that way, the existing
//! place → route → check loop treats it like any other part — the courtyard
//! becomes its placement keep-out and the through-hole header pads are reachable
//! from either copper layer.
//!
//! A bought module (the Daisy Seed) is a *rich connector*: we don't fabricate it,
//! we place the mating header pads and route the main board's nets to them. The
//! generated-2-board-stack case reuses the same footprint primitive later.
//!
//! Footprints are emitted as KiCad `.kicad_mod` text and synthesized on demand by
//! the board generator for the reserved library [`SUBBOARD_LIB`], so a part with
//! `footprint = "LobModule:Daisy_Seed"` needs no file on disk.

use crate::board::mm;

/// Reserved footprint-library name whose members are synthesized here rather than
/// read from a `.pretty` directory. A part footprint `"LobModule:<name>"` routes
/// through [`from_name`].
pub const SUBBOARD_LIB: &str = "LobModule";

/// Pad-number direction along a row: numbers increasing with Y (`Down`) or
/// decreasing with Y (`Up`). A DIP-style 2-row header numbers down one side and
/// up the other, so the two rows meet at the bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Numbering {
    Down,
    Up,
}

/// A header pin's function name and any aliases — so nets can be mapped to a
/// sub-board by function (`AUDIO_OUT_L`, `D15`) rather than pad number, and the
/// footprint can label each pad on its fab layer.
#[derive(Debug, Clone)]
pub struct PinLabel {
    pub pad: usize,
    /// Canonical name (emitted on the footprint's fab layer).
    pub name: &'static str,
    /// Other accepted names (e.g. an ADC pin's `A0` alias).
    pub aliases: &'static [&'static str],
}

/// One straight row of header pins, parallel to the Y axis at a fixed X offset
/// from the footprint origin (the body centre).
#[derive(Debug, Clone)]
pub struct PinRow {
    pub count: usize,
    pub pitch_mm: f64,
    /// Row X, relative to the body centre.
    pub x_mm: f64,
    /// Pad number of the row's **top** pin (smallest Y).
    pub first_pad: usize,
    pub numbering: Numbering,
}

impl PinRow {
    /// `(pad_number, x_mm, y_mm)` for each pin, top (−Y) to bottom (+Y). Pins are
    /// centred on the origin: a row of `count` pins spans `(count−1)·pitch`.
    fn pads(&self) -> Vec<(usize, f64, f64)> {
        let span = (self.count.saturating_sub(1)) as f64 * self.pitch_mm;
        (0..self.count)
            .map(|i| {
                let y = -span / 2.0 + i as f64 * self.pitch_mm;
                let num = match self.numbering {
                    Numbering::Down => self.first_pad + i,
                    Numbering::Up => self.first_pad + (self.count - 1 - i),
                };
                (num, self.x_mm, y)
            })
            .collect()
    }
}

/// A sub-board mounted through headers: a body outline (its courtyard/keep-out)
/// plus the header pin rows that tie it to the main board.
#[derive(Debug, Clone)]
pub struct SubboardSpec {
    pub name: String,
    pub body_w_mm: f64,
    pub body_h_mm: f64,
    pub rows: Vec<PinRow>,
    pub pad_drill_mm: f64,
    pub pad_dia_mm: f64,
    /// Function name per pad (empty for a generic unnamed header).
    pub pins: Vec<PinLabel>,
    /// Standoff (mm): how high the sub-board body sits above the main board on its
    /// headers. Components under it on the same side must be shorter than this or
    /// they collide (DESIGN 6.7).
    pub standoff_mm: f64,
}

/// What a carrier-board pin is allowed to carry. This is deliberately coarse:
/// the first consumer is validation ("raw CV into GPIO is bad"), not firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinCapability {
    Power,
    Ground,
    AudioIn,
    AudioOut,
    AudioReference,
    CvIn,
    CvOut,
    GateIn,
    GateOut,
    AnalogIn,
    Dac,
    Gpio,
    Usb,
    Storage,
}

/// A named pin on a vendored sub-board footprint. Unlike [`PinLabel`], the pad
/// id is a string because Electrosmith Patch SM / Seed2 DFM footprints use
/// alphanumeric pads (`A1`, `B2`, ...), not numeric DIP pins.
#[derive(Debug, Clone, Copy)]
pub struct ProfilePin {
    pub pad: &'static str,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub capabilities: &'static [PinCapability],
}

impl ProfilePin {
    pub fn has_capability(&self, capability: PinCapability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// A vendored module profile: semantic pin names and carrier-board capabilities
/// layered over a real embedded footprint.
#[derive(Debug, Clone, Copy)]
pub struct SubboardProfile {
    pub name: &'static str,
    pub footprint: &'static str,
    pub pins: &'static [ProfilePin],
    pub body_w_mm: Option<f64>,
    pub body_h_mm: Option<f64>,
    pub standoff_mm: Option<f64>,
    /// True when the module owns the Eurorack-level analog conditioning. Patch SM
    /// does; lower-level SOMs such as Seed2 DFM should not.
    pub eurorack_conditioned: bool,
}

impl SubboardProfile {
    /// The named profile pin for a function name, alias, or physical pad id.
    pub fn pin_for(&self, name: &str) -> Option<&ProfilePin> {
        self.pins
            .iter()
            .find(|p| {
                p.name.eq_ignore_ascii_case(name)
                    || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
            })
            .or_else(|| self.pins.iter().find(|p| p.pad.eq_ignore_ascii_case(name)))
    }

    /// The physical pad id for a function name or alias.
    pub fn pad_for(&self, name: &str) -> Option<&'static str> {
        self.pin_for(name).map(|p| p.pad)
    }

    /// The canonical function name of a pad, if named.
    pub fn pin_name(&self, pad: &str) -> Option<&'static str> {
        self.pins
            .iter()
            .find(|p| p.pad.eq_ignore_ascii_case(pad))
            .map(|p| p.name)
    }

    pub fn pins_with(&self, capability: PinCapability) -> impl Iterator<Item = &ProfilePin> + '_ {
        self.pins
            .iter()
            .filter(move |p| p.has_capability(capability))
    }
}

impl SubboardSpec {
    /// Every header pad as `(pad_number, x_mm, y_mm)`, in row-then-pin order.
    pub fn pads(&self) -> Vec<(usize, f64, f64)> {
        self.rows.iter().flat_map(|r| r.pads()).collect()
    }

    /// The pad number for a function name (case-insensitive; matches the canonical
    /// name or any alias). Lets a net be mapped to `"AUDIO_OUT_L"` not `"18"`.
    pub fn pad_for(&self, name: &str) -> Option<usize> {
        self.pins
            .iter()
            .find(|p| {
                p.name.eq_ignore_ascii_case(name)
                    || p.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
            })
            .map(|p| p.pad)
    }

    /// The canonical function name of a pad, if named.
    pub fn pin_name(&self, pad: usize) -> Option<&'static str> {
        self.pins.iter().find(|p| p.pad == pad).map(|p| p.name)
    }

    /// KiCad `.kicad_mod` text: through-hole header pads (reachable from either
    /// layer), a body-sized courtyard (the placement keep-out), and a silk body
    /// outline. Pad 1 is rectangular to mark pin 1. The board generator overwrites
    /// the name and injects placement/reference/nets, so those are placeholders.
    pub fn kicad_mod(&self) -> String {
        let (hw, hh) = (self.body_w_mm / 2.0, self.body_h_mm / 2.0);
        let mut s = String::new();
        s.push_str(&format!("(footprint \"{}\" (layer \"F.Cu\")\n", self.name));
        s.push_str(&format!(
            "  (descr \"Generated sub-board / header footprint ({} \u{d7} {} mm)\")\n",
            mm(self.body_w_mm),
            mm(self.body_h_mm)
        ));
        // Reference/value the generator fills in; placed just clear of the body.
        s.push_str(&format!(
            "  (property \"Reference\" \"REF**\" (at 0 {} 0) (layer \"F.SilkS\") (effects (font (size 1 1) (thickness 0.15))))\n",
            mm(-hh - 1.5)
        ));
        s.push_str(&format!(
            "  (property \"Value\" \"{}\" (at 0 {} 0) (layer \"F.Fab\") (effects (font (size 1 1) (thickness 0.15))))\n",
            self.name,
            mm(hh + 1.5)
        ));
        // Courtyard: only the header rows are hard keep-out — that copper is what
        // contacts the board. The body floats on its standoff, so the space
        // between the rows is left open for short parts underneath (DESIGN 6.7,
        // 25z.5); a full-body courtyard would false-trip courtyards_overlap on
        // them. One courtyard rect per pin row, sized to its pads.
        for row in &self.rows {
            let span = row.count.saturating_sub(1) as f64 * row.pitch_mm;
            let r = self.pad_dia_mm / 2.0 + 0.25;
            s.push_str(&format!(
                "  (fp_rect (start {} {}) (end {} {}) (stroke (width 0.05) (type solid)) (fill none) (layer \"F.CrtYd\"))\n",
                mm(row.x_mm - r), mm(-span / 2.0 - r), mm(row.x_mm + r), mm(span / 2.0 + r)
            ));
        }
        // Silk body outline (sits at the body edge, clear of the pads).
        s.push_str(&format!(
            "  (fp_rect (start {} {}) (end {} {}) (stroke (width 0.12) (type solid)) (fill none) (layer \"F.SilkS\"))\n",
            mm(-hw), mm(-hh), mm(hw), mm(hh)
        ));
        for (num, x, y) in self.pads() {
            let shape = if num == 1 { "rect" } else { "circle" };
            s.push_str(&format!(
                "  (pad \"{}\" thru_hole {} (at {} {}) (size {} {}) (drill {}) (layers \"*.Cu\" \"*.Mask\"))\n",
                num,
                shape,
                mm(x),
                mm(y),
                mm(self.pad_dia_mm),
                mm(self.pad_dia_mm),
                mm(self.pad_drill_mm),
            ));
        }
        // Function-name labels on the fab layer (documentation): placed just
        // inboard of each pad so the board reads which pad is which pin.
        for (num, x, y) in self.pads() {
            if let Some(name) = self.pin_name(num) {
                let inward = if x < 0.0 { 1.0 } else { -1.0 };
                let ax = x + inward * (self.pad_dia_mm / 2.0 + 0.4);
                let justify = if x < 0.0 { "left" } else { "right" };
                s.push_str(&format!(
                    "  (fp_text user \"{}\" (at {} {}) (layer \"F.Fab\") (effects (font (size 0.5 0.5) (thickness 0.08)) (justify {})))\n",
                    name, mm(ax), mm(y), justify
                ));
            }
        }
        s.push_str(")\n");
        s
    }
}

/// Electrosmith Patch Submodule pin names, matching the published Patch SM
/// pinout and libDaisy's `daisy_patch_sm.h` names.
#[rustfmt::skip]
const PATCH_SM_PINS: &[ProfilePin] = &[
    ProfilePin { pad: "A1", name: "-12V", aliases: &["VEE", "VNEG"], capabilities: &[PinCapability::Power] },
    ProfilePin { pad: "A2", name: "ADC_9", aliases: &["ADC9", "AUX_ADC_9"], capabilities: &[PinCapability::AnalogIn] },
    ProfilePin { pad: "A3", name: "ADC_10", aliases: &["ADC10", "AUX_ADC_10"], capabilities: &[PinCapability::AnalogIn] },
    ProfilePin { pad: "A4", name: "GND", aliases: &["AGND"], capabilities: &[PinCapability::Ground] },
    ProfilePin { pad: "A5", name: "+12V", aliases: &["12V", "VCC"], capabilities: &[PinCapability::Power] },
    ProfilePin { pad: "A6", name: "5V", aliases: &["V5"], capabilities: &[PinCapability::Power] },
    ProfilePin { pad: "A7", name: "GND", aliases: &["DGND"], capabilities: &[PinCapability::Ground] },
    ProfilePin { pad: "A8", name: "USB_DM", aliases: &["MIDI_TX", "USART1_TX"], capabilities: &[PinCapability::Usb] },
    ProfilePin { pad: "A9", name: "USB_DP", aliases: &["MIDI_RX", "USART1_RX"], capabilities: &[PinCapability::Usb] },
    ProfilePin { pad: "A10", name: "3V3", aliases: &["3V3_DIG", "3V3D"], capabilities: &[PinCapability::Power] },

    ProfilePin { pad: "B1", name: "AUDIO_OUT_R", aliases: &["AUDIO_OUT_RIGHT", "AUDIO_OUT_2", "OUT_R"], capabilities: &[PinCapability::AudioOut] },
    ProfilePin { pad: "B2", name: "AUDIO_OUT_L", aliases: &["AUDIO_OUT_LEFT", "AUDIO_OUT_1", "OUT_L"], capabilities: &[PinCapability::AudioOut] },
    ProfilePin { pad: "B3", name: "AUDIO_IN_R", aliases: &["AUDIO_IN_RIGHT", "AUDIO_IN_2", "IN_R"], capabilities: &[PinCapability::AudioIn] },
    ProfilePin { pad: "B4", name: "AUDIO_IN_L", aliases: &["AUDIO_IN_LEFT", "AUDIO_IN_1", "IN_L"], capabilities: &[PinCapability::AudioIn] },
    ProfilePin { pad: "B5", name: "GATE_OUT_1", aliases: &["GATEOUT1", "GATE_OUT1"], capabilities: &[PinCapability::GateOut] },
    ProfilePin { pad: "B6", name: "GATE_OUT_2", aliases: &["GATEOUT2", "GATE_OUT2"], capabilities: &[PinCapability::GateOut] },
    ProfilePin { pad: "B7", name: "I2C1_SCL", aliases: &["SCL", "SW1", "BUTTON"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B8", name: "I2C1_SDA", aliases: &["SDA", "SW2", "TOGGLE"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B9", name: "GATE_IN_2", aliases: &["GATE2", "GATE_IN2"], capabilities: &[PinCapability::GateIn] },
    ProfilePin { pad: "B10", name: "GATE_IN_1", aliases: &["GATE1", "GATE_IN1", "GATE"], capabilities: &[PinCapability::GateIn] },

    ProfilePin { pad: "C1", name: "CV_OUT_2", aliases: &["CVOUT2", "CV_OUT2"], capabilities: &[PinCapability::CvOut] },
    ProfilePin { pad: "C2", name: "CV_4", aliases: &["CV4", "CTRL_4", "KNOB4"], capabilities: &[PinCapability::CvIn] },
    ProfilePin { pad: "C3", name: "CV_3", aliases: &["CV3", "CTRL_3", "KNOB3"], capabilities: &[PinCapability::CvIn] },
    ProfilePin { pad: "C4", name: "CV_2", aliases: &["CV2", "CTRL_2", "KNOB2"], capabilities: &[PinCapability::CvIn] },
    ProfilePin { pad: "C5", name: "CV_1", aliases: &["CV1", "CTRL_1", "KNOB1", "KNOB"], capabilities: &[PinCapability::CvIn] },
    ProfilePin { pad: "C6", name: "CV_5", aliases: &["CV5", "CTRL_5", "KNOB5"], capabilities: &[PinCapability::CvIn] },
    ProfilePin { pad: "C7", name: "CV_6", aliases: &["CV6", "CTRL_6", "KNOB6"], capabilities: &[PinCapability::CvIn] },
    ProfilePin { pad: "C8", name: "CV_7", aliases: &["CV7", "CTRL_7", "KNOB7"], capabilities: &[PinCapability::CvIn] },
    ProfilePin { pad: "C9", name: "CV_8", aliases: &["CV8", "CTRL_8", "KNOB8"], capabilities: &[PinCapability::CvIn] },
    ProfilePin { pad: "C10", name: "CV_OUT_1", aliases: &["CVOUT1", "CV_OUT1", "CVOUT"], capabilities: &[PinCapability::CvOut] },

    ProfilePin { pad: "D1", name: "SPI2_CS", aliases: &["GPIO_D1"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "D2", name: "SDMMC1_D3", aliases: &["SD_D3", "USART3_RX"], capabilities: &[PinCapability::Storage] },
    ProfilePin { pad: "D3", name: "SDMMC1_D2", aliases: &["SD_D2", "USART3_TX"], capabilities: &[PinCapability::Storage] },
    ProfilePin { pad: "D4", name: "SDMMC1_D1", aliases: &["SD_D1"], capabilities: &[PinCapability::Storage] },
    ProfilePin { pad: "D5", name: "SDMMC1_D0", aliases: &["SD_D0"], capabilities: &[PinCapability::Storage] },
    ProfilePin { pad: "D6", name: "SDMMC1_CLK", aliases: &["SD_CLK", "UART5_TX"], capabilities: &[PinCapability::Storage] },
    ProfilePin { pad: "D7", name: "SDMMC1_CMD", aliases: &["SD_CMD", "UART5_RX"], capabilities: &[PinCapability::Storage] },
    ProfilePin { pad: "D8", name: "ADC_12", aliases: &["ADC12"], capabilities: &[PinCapability::AnalogIn] },
    ProfilePin { pad: "D9", name: "ADC_11", aliases: &["ADC11"], capabilities: &[PinCapability::AnalogIn] },
    ProfilePin { pad: "D10", name: "SPI2_SCK", aliases: &["GPIO_D10"], capabilities: &[PinCapability::Gpio] },
];

/// Electrosmith Seed2 DFM pin names from the published Seed2 DFM pinout CSV /
/// datasheet v1.0.12. The module exposes raw codec I/O and MCU pins; the carrier
/// is responsible for application-level analog conditioning.
#[rustfmt::skip]
const SEED2_DFM_PINS: &[ProfilePin] = &[
    ProfilePin { pad: "A1", name: "VIN", aliases: &["VIN_1"], capabilities: &[PinCapability::Power] },
    ProfilePin { pad: "A2", name: "VIN", aliases: &["VIN_2"], capabilities: &[PinCapability::Power] },
    ProfilePin { pad: "A3", name: "3V3_D", aliases: &["3V3D", "+3V3D", "+3V3_D"], capabilities: &[PinCapability::Power] },
    ProfilePin { pad: "A4", name: "3V3_D", aliases: &["3V3D_2", "+3V3D_2", "+3V3_D_2"], capabilities: &[PinCapability::Power] },
    ProfilePin { pad: "A5", name: "3V3_A", aliases: &["3V3A", "+3V3A", "+3V3_A"], capabilities: &[PinCapability::Power] },
    ProfilePin { pad: "A6", name: "GND", aliases: &["GND_1"], capabilities: &[PinCapability::Ground] },
    ProfilePin { pad: "A7", name: "GND", aliases: &["GND_2"], capabilities: &[PinCapability::Ground] },
    ProfilePin { pad: "A8", name: "GND", aliases: &["GND_3"], capabilities: &[PinCapability::Ground] },
    ProfilePin { pad: "A9", name: "GND", aliases: &["GND_4"], capabilities: &[PinCapability::Ground] },
    ProfilePin { pad: "A10", name: "GND", aliases: &["GND_5"], capabilities: &[PinCapability::Ground] },

    ProfilePin { pad: "B1", name: "D9", aliases: &["PB4", "SPI1_MISO", "UART7_TX", "I2S1_SDI", "SPI3_MISO", "I2S3_SDI", "SPI6_MISO"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B2", name: "D8", aliases: &["PG11", "SPI1_SCK", "I2S1_CK", "LPTIM1_IN2", "HRTIM_EEV4"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B3", name: "D13", aliases: &["PB6", "USART1_TX", "LPUART1_TX", "UART5_TX", "I2C1_SCL", "I2C4_SCL", "TIM16_CH1N", "TIM4_CH1"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B4", name: "D10", aliases: &["PB5", "SPI1_MOSI", "UART5_RX", "I2S1_SDO", "SPI3_MOSI", "I2S3_SDO", "SPI6_MOSI", "I2C4_SMBA", "TIM17_BKIN"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B5", name: "D14", aliases: &["PB7", "USART1_RX", "LPUART1_RX", "I2C1_SDA", "I2C4_SDA", "TIM17_CH1N", "TIM4_CH2"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B6", name: "D7", aliases: &["PG10", "SPI1_NSS", "I2S1_WS", "HRTIM_FLT5"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B7", name: "D11", aliases: &["PB8", "I2C1_SCL", "UART4_RX", "I2C4_SCL", "TIM16_CH1", "TIM4_CH3"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B8", name: "D12", aliases: &["PB9", "I2C1_SDA", "UART4_TX", "SPI2_NSS", "I2S2_WS", "I2C4_SDA", "I2C4_SMBA", "TIM17_CH1", "TIM4_CH4"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "B9", name: "D30", aliases: &["PB15", "USB_HS_D+", "USB_HS_DP", "USART1_RX"], capabilities: &[PinCapability::Gpio, PinCapability::Usb] },
    ProfilePin { pad: "B10", name: "D29", aliases: &["PB14", "USB_HS_D-", "USB_HS_DM", "USART_1_TX", "USART1_TX", "TIM1_CH2N"], capabilities: &[PinCapability::Gpio, PinCapability::Usb] },

    ProfilePin { pad: "C1", name: "D16", aliases: &["A1", "PA3", "ADC1", "USART2_RX", "TIM2_CH4", "TIM5_CH4"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "C2", name: "D19", aliases: &["A4", "PA6", "ADC4", "SPI1_MISO", "I2S1_SDI", "SPI6_MISO", "TIM1_BKIN", "TIM3_CH1"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "C3", name: "D20", aliases: &["A5", "PC1", "ADC5"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "C4", name: "D18", aliases: &["A3", "PA7", "ADC3", "SPI1_MOSI", "I2S1_SDO", "SPI6_MOSI", "TIM1_CH1N", "TIM3_CH2"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "C5", name: "D17", aliases: &["A2", "PB1", "ADC2", "TIM1_CH3N", "TIM3_CH4"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "C6", name: "D21", aliases: &["A6", "PC4", "ADC6", "I2S1_MCK"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "C7", name: "D15", aliases: &["A0", "PC0", "ADC0", "SAI2_FS_B"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "C8", name: "D23", aliases: &["A8", "PA4", "DAC1", "ADC8", "SPI1_NSS", "I2S1_WS", "SPI3_NSS", "I2S3_WS", "SPI6_NSS", "D1PWREN"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn, PinCapability::Dac] },
    ProfilePin { pad: "C9", name: "D22", aliases: &["A7", "PA5", "DAC2", "ADC7", "SPI1_SCK", "I2S1_CK", "SPI6_SCK", "D2PWREN", "TIM2_CH1"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn, PinCapability::Dac] },
    ProfilePin { pad: "C10", name: "D31", aliases: &["A12", "PC2", "SPI2_MISO", "ADC12"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },

    ProfilePin { pad: "D1", name: "AUDIO_IN_L", aliases: &["AUDIO_IN_LEFT", "IN_L"], capabilities: &[PinCapability::AudioIn] },
    ProfilePin { pad: "D2", name: "AUDIO_IN_R", aliases: &["AUDIO_IN_RIGHT", "IN_R"], capabilities: &[PinCapability::AudioIn] },
    ProfilePin { pad: "D3", name: "AUDIO_VCOM", aliases: &["VCOM", "AUDIO_COMMON"], capabilities: &[PinCapability::AudioReference] },
    ProfilePin { pad: "D4", name: "AUDIO_OUT_L_NEG", aliases: &["AUDIO_OUT_L-", "AUDIO_OUT_L_N", "AUDIO_OUT_LEFT_NEG"], capabilities: &[PinCapability::AudioOut] },
    ProfilePin { pad: "D5", name: "AUDIO_OUT_L_POS", aliases: &["AUDIO_OUT_L+", "AUDIO_OUT_L_P", "AUDIO_OUT_LEFT_POS"], capabilities: &[PinCapability::AudioOut] },
    ProfilePin { pad: "D6", name: "AUDIO_OUT_R_POS", aliases: &["AUDIO_OUT_R+", "AUDIO_OUT_R_P", "AUDIO_OUT_RIGHT_POS"], capabilities: &[PinCapability::AudioOut] },
    ProfilePin { pad: "D7", name: "AUDIO_OUT_R_NEG", aliases: &["AUDIO_OUT_R-", "AUDIO_OUT_R_N", "AUDIO_OUT_RIGHT_NEG"], capabilities: &[PinCapability::AudioOut] },
    ProfilePin { pad: "D8", name: "D32", aliases: &["A13", "PC3", "SPI2_MOSI", "ADC13"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "D9", name: "D0", aliases: &["PB12", "USB_HS_ID", "UART5_RX", "USART3_CK", "TIM1_BKIN"], capabilities: &[PinCapability::Gpio, PinCapability::Usb] },
    ProfilePin { pad: "D10", name: "D27", aliases: &["PG9", "SAI2_FS_B", "USART6_RX", "SPI1_MISO", "I2S1_SDI"], capabilities: &[PinCapability::Gpio] },

    ProfilePin { pad: "E1", name: "D24", aliases: &["A9", "PA1", "ADC9", "SAI2_MCLK_B", "UART4_RX", "TIM2_CH2", "TIM5_CH2"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "E2", name: "D25", aliases: &["A10", "PA0", "ADC10", "SAI2_SD_B", "UART4_TX", "TIM2_CH1", "TIM2_ETR", "TIM5_CH1"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "E3", name: "D26", aliases: &["PD11", "SAI2_SD_A", "I2C4_SMBA", "LPTIM2_IN2"], capabilities: &[PinCapability::Gpio] },
    ProfilePin { pad: "E4", name: "D28", aliases: &["A11", "PA2", "ADC11", "SAI2_SCK_B", "USART2_TX", "TIM2_CH3", "TIM5_CH3"], capabilities: &[PinCapability::Gpio, PinCapability::AnalogIn] },
    ProfilePin { pad: "E5", name: "D6", aliases: &["PC12", "SDMMC_CK", "SD_CLK", "UART5_TX", "USART3_CK", "SPI3_MOSI", "I2S3_SDO"], capabilities: &[PinCapability::Gpio, PinCapability::Storage] },
    ProfilePin { pad: "E6", name: "D5", aliases: &["PD2", "SDMMC_CMD", "SD_CMD", "UART5_RX"], capabilities: &[PinCapability::Gpio, PinCapability::Storage] },
    ProfilePin { pad: "E7", name: "D4", aliases: &["PC8", "SDMMC_D0", "SD_D0", "UART5_CTS"], capabilities: &[PinCapability::Gpio, PinCapability::Storage] },
    ProfilePin { pad: "E8", name: "D3", aliases: &["PC9", "SDMMC_D1", "SD_D1", "UART5_CTS", "I2S_CKIN", "MCO2"], capabilities: &[PinCapability::Gpio, PinCapability::Storage] },
    ProfilePin { pad: "E9", name: "D2", aliases: &["PC10", "SDMMC_D2", "SD_D2", "USART3_TX", "UART4_TX", "SPI3_SCK", "I2S3_CK", "HRTIM_EEV1"], capabilities: &[PinCapability::Gpio, PinCapability::Storage] },
    ProfilePin { pad: "E10", name: "D1", aliases: &["PC11", "SDMMC_D3", "SD_D3", "USART3_RX", "UART4_RX", "SPI3_MISO", "I2S3_SDI", "HRTIM_FLT2"], capabilities: &[PinCapability::Gpio, PinCapability::Storage] },
];

pub fn profile(name: &str) -> Option<SubboardProfile> {
    match name {
        "DAISY_PATCH_SM" => Some(SubboardProfile {
            name: "Daisy Patch SM",
            footprint: "DAISY_PATCH_SM",
            pins: PATCH_SM_PINS,
            body_w_mm: None,
            body_h_mm: None,
            standoff_mm: Some(8.5),
            eurorack_conditioned: true,
        }),
        "DAISY_PATCH_SM_SMT" => Some(SubboardProfile {
            name: "Daisy Patch SM",
            footprint: "DAISY_PATCH_SM_SMT",
            pins: PATCH_SM_PINS,
            body_w_mm: None,
            body_h_mm: None,
            standoff_mm: Some(8.5),
            eurorack_conditioned: true,
        }),
        "DAISY_SEED2_DFM" => Some(SubboardProfile {
            name: "Daisy Seed2 DFM",
            footprint: "DAISY_SEED2_DFM",
            pins: SEED2_DFM_PINS,
            body_w_mm: Some(55.0),
            body_h_mm: Some(28.0),
            standoff_mm: None,
            eurorack_conditioned: false,
        }),
        "DAISY_SEED2_DFM_PTH" => Some(SubboardProfile {
            name: "Daisy Seed2 DFM",
            footprint: "DAISY_SEED2_DFM_PTH",
            pins: SEED2_DFM_PINS,
            body_w_mm: Some(55.0),
            body_h_mm: Some(28.0),
            standoff_mm: None,
            eurorack_conditioned: false,
        }),
        _ => None,
    }
}

/// The Electrosmith **Daisy Seed** as a placed sub-module: an 18 × 51 mm board on
/// two 1×20 headers, 2.54 mm pitch, rows 15.24 mm (0.6") apart. Pads number down
/// the left (1–20) and up the right (21–40), meeting at the bottom — DIP
/// convention. (Exact pin↔function labels are the author's net map; verify the
/// physical numbering against the Daisy datasheet before fabricating.)
pub fn daisy_seed() -> SubboardSpec {
    const PITCH: f64 = 2.54;
    const ROW_DX: f64 = 15.24 / 2.0; // ±7.62 mm from centre (0.6" apart)
    SubboardSpec {
        name: "Daisy_Seed".into(),
        body_w_mm: 18.0,
        body_h_mm: 51.0,
        rows: vec![
            PinRow {
                count: 20,
                pitch_mm: PITCH,
                x_mm: -ROW_DX,
                first_pad: 1,
                numbering: Numbering::Down,
            },
            PinRow {
                count: 20,
                pitch_mm: PITCH,
                x_mm: ROW_DX,
                first_pad: 21,
                numbering: Numbering::Up,
            },
        ],
        pad_drill_mm: 1.0,
        pad_dia_mm: 1.7,
        pins: DAISY_SEED_PINS
            .iter()
            .map(|&(pad, name, aliases)| PinLabel { pad, name, aliases })
            .collect(),
        // A Daisy Seed typically stacks on ~11 mm 2×20 headers.
        standoff_mm: 11.0,
    }
}

/// Daisy Seed physical pinout (pad number → function name + aliases), from the
/// official Electrosmith libDaisy `Daisy_Seed_Rev4_Pinout.csv`. Pad numbers match
/// the DIP layout above (1 top-left → 20 bottom-left, 21 bottom-right → 40
/// top-right). ADC-capable GPIOs carry their `A#` alias.
#[rustfmt::skip]
const DAISY_SEED_PINS: &[(usize, &str, &[&str])] = &[
    (1, "D0", &[]),  (2, "D1", &[]),  (3, "D2", &[]),  (4, "D3", &[]),  (5, "D4", &[]),
    (6, "D5", &[]),  (7, "D6", &[]),  (8, "D7", &[]),  (9, "D8", &[]),  (10, "D9", &[]),
    (11, "D10", &[]), (12, "D11", &[]), (13, "D12", &[]), (14, "D13", &[]), (15, "D14", &[]),
    (16, "AUDIO_IN_L", &[]), (17, "AUDIO_IN_R", &[]),
    (18, "AUDIO_OUT_L", &[]), (19, "AUDIO_OUT_R", &[]),
    (20, "AGND", &[]),
    (21, "3V3_ANA", &["3V3A"]),
    (22, "D15", &["A0"]), (23, "D16", &["A1"]), (24, "D17", &["A2"]), (25, "D18", &["A3"]),
    (26, "D19", &["A4"]), (27, "D20", &["A5"]), (28, "D21", &["A6"]), (29, "D22", &["A7"]),
    (30, "D23", &["A8"]), (31, "D24", &["A9"]), (32, "D25", &["A10"]), (33, "D26", &[]),
    (34, "D27", &[]), (35, "D28", &["A11"]), (36, "D29", &[]), (37, "D30", &[]),
    (38, "3V3_DIG", &["3V3D"]), (39, "VIN", &[]), (40, "GND", &["DGND"]),
];

/// Resolve a synthesized sub-board footprint by name (the part after
/// `LobModule:`). `None` for an unknown name. Only modules we synthesize (with a
/// standoff + named pins) have a spec; vendored real footprints do not.
pub fn from_name(name: &str) -> Option<SubboardSpec> {
    match name {
        "Daisy_Seed" => Some(daisy_seed()),
        _ => None,
    }
}

/// The `.kicad_mod` text for a `LobModule:<name>` footprint — either synthesized
/// (the Daisy Seed) or a **real, vendored** footprint embedded from Electrosmith's
/// MIT-licensed DaisyKiCad library (Patch SM, Seed2 DFM). Vendored footprints name
/// their pads by the physical Daisy pin (`A1`…`D10` / `A1`…`E10`), so a net wired
/// to pin `B2` connects straight through. `None` for an unknown name.
pub fn footprint_text(name: &str) -> Option<String> {
    let vendored = |s: &str| Some(s.to_string());
    match name {
        "Daisy_Seed" => Some(daisy_seed().kicad_mod()),
        "DAISY_PATCH_SM" => vendored(include_str!(
            "../footprints/Daisy-Boards.pretty/DAISY_PATCH_SM.kicad_mod"
        )),
        "DAISY_PATCH_SM_SMT" => vendored(include_str!(
            "../footprints/Daisy-Boards.pretty/DAISY_PATCH_SM_SMT.kicad_mod"
        )),
        "DAISY_SEED2_DFM" => vendored(include_str!(
            "../footprints/Daisy-Boards.pretty/DAISY_SEED2_DFM.kicad_mod"
        )),
        "DAISY_SEED2_DFM_PTH" => vendored(include_str!(
            "../footprints/Daisy-Boards.pretty/DAISY_SEED2_DFM_PTH.kicad_mod"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sexpr::Sexpr;

    #[test]
    fn daisy_has_40_pads_and_the_right_body() {
        let d = daisy_seed();
        let pads = d.pads();
        assert_eq!(pads.len(), 40, "Daisy Seed is a 2×20 header");
        // Pad numbers 1..=40 each appear exactly once.
        let mut nums: Vec<usize> = pads.iter().map(|(n, _, _)| *n).collect();
        nums.sort_unstable();
        assert_eq!(nums, (1..=40).collect::<Vec<_>>());
        // Body is 18 × 51 mm.
        assert_eq!((d.body_w_mm, d.body_h_mm), (18.0, 51.0));
    }

    #[test]
    fn dip_numbering_meets_at_the_bottom() {
        // Pad 1 (top-left) and pad 40 (top-right) are both at the top (min Y);
        // pad 20 (bottom-left) and pad 21 (bottom-right) at the bottom (max Y).
        let pads = daisy_seed().pads();
        let y = |n: usize| pads.iter().find(|(p, _, _)| *p == n).unwrap().2;
        assert!(y(1) < 0.0 && y(40) < 0.0, "pins 1 and 40 sit at the top");
        assert!(
            y(20) > 0.0 && y(21) > 0.0,
            "pins 20 and 21 sit at the bottom"
        );
        assert!((y(1) - y(40)).abs() < 1e-9, "top pins share a row Y");
        assert!((y(20) - y(21)).abs() < 1e-9, "bottom pins share a row Y");
    }

    #[test]
    fn kicad_mod_parses_and_carries_pads_and_courtyard() {
        let text = daisy_seed().kicad_mod();
        let fp = Sexpr::parse(&text).expect("generated footprint must parse");
        let items = fp.as_list().unwrap();
        assert_eq!(items[0].as_atom(), Some("footprint"));
        let pads = items.iter().filter(|i| i.head() == Some("pad")).count();
        assert_eq!(pads, 40);
        assert!(text.contains("F.CrtYd"), "declares a courtyard keep-out");
        assert!(
            text.contains(r#"(pad "1" thru_hole rect"#),
            "pin 1 is marked"
        );
        assert!(text.contains("*.Cu"), "header pads reach both layers");
    }

    #[test]
    fn named_pins_resolve_by_function_and_alias() {
        let d = daisy_seed();
        // All 40 pads are named exactly once.
        assert_eq!(d.pins.len(), 40);
        // Canonical names map to the right physical pad.
        assert_eq!(d.pad_for("AUDIO_OUT_L"), Some(18));
        assert_eq!(d.pad_for("D0"), Some(1));
        assert_eq!(d.pad_for("GND"), Some(40));
        assert_eq!(d.pad_for("D15"), Some(22));
        // Case-insensitive + ADC aliases.
        assert_eq!(d.pad_for("audio_out_l"), Some(18));
        assert_eq!(d.pad_for("A0"), Some(22), "ADC alias resolves");
        assert_eq!(d.pad_for("DGND"), Some(40));
        assert_eq!(d.pad_for("nope"), None);
        // Reverse lookup.
        assert_eq!(d.pin_name(18), Some("AUDIO_OUT_L"));
    }

    #[test]
    fn patch_sm_profile_resolves_carrier_signal_names() {
        let p = profile("DAISY_PATCH_SM").expect("Patch SM has a profile");
        assert_eq!(p.footprint, "DAISY_PATCH_SM");
        assert!(p.eurorack_conditioned);
        assert_eq!(p.pins.len(), 40);

        assert_eq!(p.pad_for("AUDIO_IN_L"), Some("B4"));
        assert_eq!(p.pad_for("audio_out_left"), Some("B2"));
        assert_eq!(p.pad_for("CV_1"), Some("C5"));
        assert_eq!(p.pad_for("knob1"), Some("C5"));
        assert_eq!(p.pad_for("GATE"), Some("B10"));
        assert_eq!(p.pin_name("C10"), Some("CV_OUT_1"));

        assert_eq!(p.pins_with(PinCapability::AudioIn).count(), 2);
        assert_eq!(p.pins_with(PinCapability::AudioOut).count(), 2);
        assert_eq!(p.pins_with(PinCapability::CvIn).count(), 8);
        assert_eq!(p.pins_with(PinCapability::CvOut).count(), 2);
        assert_eq!(p.pins_with(PinCapability::GateIn).count(), 2);
        assert_eq!(p.pins_with(PinCapability::GateOut).count(), 2);
    }

    #[test]
    fn seed2_dfm_profile_resolves_carrier_signal_names() {
        let p = profile("DAISY_SEED2_DFM").expect("Seed2 DFM has a profile");
        assert_eq!(p.footprint, "DAISY_SEED2_DFM");
        assert_eq!(p.body_w_mm, Some(55.0));
        assert_eq!(p.body_h_mm, Some(28.0));
        assert_eq!(p.standoff_mm, None);
        assert!(!p.eurorack_conditioned);
        assert_eq!(p.pins.len(), 50);

        assert_eq!(p.pad_for("VIN"), Some("A1"));
        assert_eq!(p.pad_for("D16"), Some("C1"));
        assert_eq!(p.pad_for("A1"), Some("C1"));
        assert_eq!(p.pad_for("ADC13"), Some("D8"));
        assert_eq!(p.pad_for("DAC1"), Some("C8"));
        assert_eq!(p.pad_for("AUDIO_OUT_L+"), Some("D5"));
        assert_eq!(p.pad_for("USB_HS_DP"), Some("B9"));
        assert_eq!(p.pad_for("SDMMC_D3"), Some("E10"));
        assert_eq!(p.pin_name("D4"), Some("AUDIO_OUT_L_NEG"));

        assert_eq!(p.pins_with(PinCapability::Power).count(), 5);
        assert_eq!(p.pins_with(PinCapability::Ground).count(), 5);
        assert_eq!(p.pins_with(PinCapability::AudioIn).count(), 2);
        assert_eq!(p.pins_with(PinCapability::AudioOut).count(), 4);
        assert_eq!(p.pins_with(PinCapability::AudioReference).count(), 1);
        assert_eq!(p.pins_with(PinCapability::AnalogIn).count(), 14);
        assert_eq!(p.pins_with(PinCapability::Dac).count(), 2);
        assert_eq!(p.pins_with(PinCapability::Gpio).count(), 33);
        assert_eq!(p.pins_with(PinCapability::Usb).count(), 3);
        assert_eq!(p.pins_with(PinCapability::Storage).count(), 6);
    }

    #[test]
    fn seed2_dfm_pth_profile_uses_the_same_pin_map() {
        let p = profile("DAISY_SEED2_DFM_PTH").expect("Seed2 DFM PTH has a profile");
        assert_eq!(p.footprint, "DAISY_SEED2_DFM_PTH");
        assert_eq!(p.pins.len(), 50);
        assert_eq!(p.pad_for("D32"), Some("D8"));
        assert_eq!(p.pad_for("A13"), Some("D8"));
    }

    #[test]
    fn kicad_mod_labels_pads_on_the_fab_layer() {
        let text = daisy_seed().kicad_mod();
        assert!(text.contains("F.Fab"), "pin names documented on fab");
        assert!(text.contains(r#"(fp_text user "AUDIO_OUT_L""#));
        assert!(text.contains(r#"(fp_text user "VIN""#));
    }
}
