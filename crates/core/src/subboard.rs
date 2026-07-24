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
}

impl SubboardSpec {
    /// Every header pad as `(pad_number, x_mm, y_mm)`, in row-then-pin order.
    pub fn pads(&self) -> Vec<(usize, f64, f64)> {
        self.rows.iter().flat_map(|r| r.pads()).collect()
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
        // Courtyard = the sub-board body → the placement keep-out.
        s.push_str(&format!(
            "  (fp_rect (start {} {}) (end {} {}) (stroke (width 0.05) (type solid)) (fill none) (layer \"F.CrtYd\"))\n",
            mm(-hw), mm(-hh), mm(hw), mm(hh)
        ));
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
    }
}

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
}
