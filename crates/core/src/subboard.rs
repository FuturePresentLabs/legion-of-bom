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
/// `LobModule:`). `None` for an unknown name.
pub fn from_name(name: &str) -> Option<SubboardSpec> {
    match name {
        "Daisy_Seed" => Some(daisy_seed()),
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
    fn kicad_mod_labels_pads_on_the_fab_layer() {
        let text = daisy_seed().kicad_mod();
        assert!(text.contains("F.Fab"), "pin names documented on fab");
        assert!(text.contains(r#"(fp_text user "AUDIO_OUT_L""#));
        assert!(text.contains(r#"(fp_text user "VIN""#));
    }
}
