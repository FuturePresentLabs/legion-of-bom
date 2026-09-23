//! Hand placement — where *you* want the jacks and pots, in panel millimetres.
//!
//! The pipeline's placers are automatic: [`EurorackPlacer`](crate::board::EurorackPlacer)
//! anchors panel controls to a panel spec's cutouts and packs everything else
//! around them. That is a fine starting point and a bad final answer, because
//! panel ergonomics are a design decision — which jack sits under which knob,
//! how the strip is spaced, what lines up with what — and no cost function has
//! an opinion about it.
//!
//! So this file is the human's input to the layout. It is read in **panel
//! space**: millimetres from the panel's bottom-left, which is how you think
//! about a front panel and how the panel spec already reads. The board then
//! follows the placement, and the panel is derived back from the board
//! ([`panel_from_board`](crate::panel::panel_from_board)) — so the artifact you
//! hand-authored and the artifact you manufacture cannot drift apart.
//!
//! ```toml
//! # slew_limiter.placement.toml
//! [controls]
//! RV1 = { x = 12.7, y = 113.5 }
//!
//! [[patterns.column]]
//! refdes = ["J1", "J2", "J4"]
//! x      = 6.0
//! from_y = 10.5
//! pitch  = 13.5
//!
//! [[patterns.grid]]
//! refdes  = ["SW1", "SW2", "SW3", "SW4"]
//! x       = 6.0
//! y       = 60.0
//! cols    = 2
//! pitch_x = 12.0
//! pitch_y = 12.0
//! ```
//!
//! Patterns expand to positions; anything named in `[controls]` overrides the
//! pattern that produced it, so a strip can be laid out in one line and a single
//! awkward part nudged without unpicking it.

use std::collections::HashMap;

/// A hand-authored placement file.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct PlacementFile {
    /// Panel width in HP. Optional — the board's own outline decides the panel
    /// width once it exists; this is only a hint for the first build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hp: Option<u16>,
    /// Explicit positions, refdes → point. Overrides any pattern.
    #[serde(default)]
    pub controls: HashMap<String, Point>,
    #[serde(default)]
    pub patterns: Patterns,
}

/// A position in panel space: millimetres from the panel's bottom-left.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// The generators. Each is an array, so a panel can carry several strips.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct Patterns {
    /// Evenly spaced up a vertical line — the Eurorack jack strip.
    #[serde(default)]
    pub column: Vec<Column>,
    /// Evenly spaced along a horizontal line.
    #[serde(default)]
    pub row: Vec<Row>,
    /// A rectangular array, filled left-to-right then downward.
    #[serde(default)]
    pub grid: Vec<Grid>,
}

/// `refdes` stacked up from `from_y` at `pitch`, all at `x`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct Column {
    pub refdes: Vec<String>,
    pub x: f64,
    pub from_y: f64,
    pub pitch: f64,
}

/// `refdes` spread right from `from_x` at `pitch`, all at `y`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct Row {
    pub refdes: Vec<String>,
    pub y: f64,
    pub from_x: f64,
    pub pitch: f64,
}

/// `refdes` filled across `cols` columns, left-to-right then downward from
/// `(x, y)` — the top-left cell, since a grid is read the way it is written.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct Grid {
    pub refdes: Vec<String>,
    pub x: f64,
    pub y: f64,
    pub cols: usize,
    pub pitch_x: f64,
    pub pitch_y: f64,
}

/// What went wrong reading a placement file.
#[derive(Debug, thiserror::Error)]
pub enum PlacementError {
    #[error("placement file is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0} is placed twice by different patterns")]
    Duplicate(String),
    #[error("grid for {0:?} needs cols >= 1")]
    EmptyGrid(String),
}

impl PlacementFile {
    /// Parse from TOML.
    pub fn from_toml(s: &str) -> Result<Self, PlacementError> {
        Ok(toml::from_str(s)?)
    }

    /// Expand every pattern and apply the explicit overrides, yielding one
    /// panel-space point per named reference designator.
    ///
    /// A refdes produced by two different patterns is an error rather than a
    /// last-one-wins: two patterns disagreeing about where a jack goes is a
    /// mistake in the file, and silently picking one puts a hole in the panel
    /// that the board will not meet. An explicit `[controls]` entry is *not* a
    /// conflict — that is the documented way to nudge one part out of a strip.
    pub fn positions(&self) -> Result<HashMap<String, Point>, PlacementError> {
        let mut out: HashMap<String, Point> = HashMap::new();
        let claim = |refdes: &str, p: Point, out: &mut HashMap<String, Point>| {
            if out.insert(refdes.to_string(), p).is_some() {
                return Err(PlacementError::Duplicate(refdes.to_string()));
            }
            Ok(())
        };

        for c in &self.patterns.column {
            for (i, r) in c.refdes.iter().enumerate() {
                let p = Point {
                    x: c.x,
                    y: c.from_y + c.pitch * i as f64,
                };
                claim(r, p, &mut out)?;
            }
        }
        for r0 in &self.patterns.row {
            for (i, r) in r0.refdes.iter().enumerate() {
                let p = Point {
                    x: r0.from_x + r0.pitch * i as f64,
                    y: r0.y,
                };
                claim(r, p, &mut out)?;
            }
        }
        for g in &self.patterns.grid {
            if g.cols == 0 {
                return Err(PlacementError::EmptyGrid(g.refdes.join(", ")));
            }
            for (i, r) in g.refdes.iter().enumerate() {
                let (col, row) = (i % g.cols, i / g.cols);
                let p = Point {
                    x: g.x + g.pitch_x * col as f64,
                    // Downward on the panel is decreasing y, since panel space
                    // measures up from the bottom edge.
                    y: g.y - g.pitch_y * row as f64,
                };
                claim(r, p, &mut out)?;
            }
        }
        // Explicit wins, and is allowed to overwrite a pattern's answer.
        for (refdes, p) in &self.controls {
            out.insert(refdes.clone(), *p);
        }
        Ok(out)
    }

    /// Panel-space positions converted to the board-local anchors
    /// [`EurorackPlacer`](crate::board::EurorackPlacer) consumes: same x,
    /// y measured down from the top instead of up from the bottom.
    ///
    /// This is the same flip [`panel_from_board`](crate::panel::panel_from_board)
    /// undoes, which is what keeps the hand-authored file, the board and the
    /// derived panel describing one layout rather than three.
    pub fn anchors(
        &self,
        panel_height_mm: f64,
    ) -> Result<HashMap<String, (f64, f64)>, PlacementError> {
        Ok(self
            .positions()?
            .into_iter()
            .map(|(r, p)| (r, (p.x, panel_height_mm - p.y)))
            .collect())
    }
}

/// One part of a built board, seen the way the placement editor needs to see it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedControl {
    pub refdes: String,
    pub value: String,
    pub footprint: String,
    /// The **mount point** in panel space — where the shaft, barrel or lens comes
    /// through the panel, not where the footprint's origin sits. Those differ by
    /// several millimetres on an Alpha pot, and this is the one the placement file
    /// stores, so they must not be confused.
    pub point: Point,
    pub rotation_deg: f64,
    pub back: bool,
    /// The cutout this part needs, if it is panel hardware. `None` = board-only:
    /// the placer owns it and the editor draws it as context.
    pub cutout: Option<crate::panel::CutoutSpec>,
}

/// A built board expressed in panel space — the read half of hand placement.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardView {
    pub width_mm: f64,
    pub height_mm: f64,
    /// Panel width in HP, from the board's own outline. The board decides the
    /// panel width, not the other way round.
    pub hp: u16,
    /// Every part, panel hardware and board internals alike, sorted by refdes.
    pub parts: Vec<PlacedControl>,
}

impl BoardView {
    /// Just the panel hardware — the parts a person has an opinion about.
    pub fn controls(&self) -> impl Iterator<Item = &PlacedControl> {
        self.parts.iter().filter(|p| p.cutout.is_some())
    }
}

/// Read a built board back into panel space.
///
/// This is the inverse of [`PlacementFile::anchors`] and it is deliberately
/// computed the same way [`panel_from_board`](crate::panel::panel_from_board)
/// computes it — from the hardware's bounding-box centre, relative to the
/// `Edge.Cuts` outline, with Y flipped. Any other convention would make the
/// editor show a knob where the board does not have one, or write a placement
/// that moves a part it was only meant to report.
///
/// Board internals are included (with `cutout: None`) rather than filtered out:
/// the editor draws them dimmed, because placing a jack strip without seeing what
/// is underneath it is how a control ends up on top of an op-amp.
pub fn board_view(
    board_pcb: &str,
    circuit: &dyn crate::source::CircuitSource,
    cutouts: &dyn crate::panel::CutoutSource,
) -> Result<BoardView, String> {
    /// Eurorack HP. Local to keep this file free of a panel-format dependency it
    /// does not otherwise need; the panel module owns the same constant.
    const HP_MM: f64 = 5.08;

    let placed = crate::guide::parse_board(board_pcb)?;
    let (x0, y0, x1, y1) =
        crate::guide::board_outline(board_pcb).ok_or("board has no Edge.Cuts outline")?;
    let (w, h) = ((x1 - x0).abs(), (y1 - y0).abs());
    if w <= 0.0 || h <= 0.0 {
        return Err("board outline has no area".into());
    }

    let mpn_of = |refdes: &str| {
        circuit
            .parts()
            .iter()
            .find(|p| p.refdes.0 == refdes)
            .and_then(|p| p.mpn.clone())
    };

    let mut parts: Vec<PlacedControl> = placed
        .iter()
        .map(|p| {
            // The hardware sits at the courtyard centre; the footprint origin can
            // be millimetres away (an Alpha pot's origin is pin 1).
            let (hx, hy) = ((p.bbox.0 + p.bbox.2) / 2.0, (p.bbox.1 + p.bbox.3) / 2.0);
            let cutout = crate::guide::is_panel_mounted(p)
                .then(|| cutouts.cutout(mpn_of(&p.refdes).as_deref(), &p.footprint))
                .flatten();
            PlacedControl {
                refdes: p.refdes.clone(),
                value: p.value.clone(),
                footprint: p.footprint.clone(),
                point: Point {
                    x: hx - x0,
                    y: h - (hy - y0),
                },
                rotation_deg: p.rotation_deg,
                back: p.back,
                cutout,
            }
        })
        .collect();
    parts.sort_by(|a, b| a.refdes.cmp(&b.refdes));

    Ok(BoardView {
        width_mm: w,
        height_mm: h,
        hp: ((w / HP_MM).round() as u16).max(1),
        parts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
hp = 5

[controls]
RV1 = { x = 12.7, y = 113.5 }

[[patterns.column]]
refdes = ["J1", "J2", "J4"]
x      = 6.0
from_y = 10.5
pitch  = 13.5

[[patterns.grid]]
refdes  = ["SW1", "SW2", "SW3", "SW4"]
x       = 6.0
y       = 60.0
cols    = 2
pitch_x = 12.0
pitch_y = 12.0
"#;

    #[test]
    fn a_column_stacks_upward_at_pitch() {
        let f = PlacementFile::from_toml(FILE).unwrap();
        let p = f.positions().unwrap();
        assert_eq!(f.hp, Some(5));
        assert_eq!(p["J1"], Point { x: 6.0, y: 10.5 });
        assert_eq!(p["J2"], Point { x: 6.0, y: 24.0 });
        assert_eq!(p["J4"], Point { x: 6.0, y: 37.5 });
    }

    #[test]
    fn a_grid_fills_across_then_down_the_panel() {
        let p = PlacementFile::from_toml(FILE).unwrap().positions().unwrap();
        // Row 0 across…
        assert_eq!(p["SW1"], Point { x: 6.0, y: 60.0 });
        assert_eq!(p["SW2"], Point { x: 18.0, y: 60.0 });
        // …then down, which on a panel means a *lower* y.
        assert_eq!(p["SW3"], Point { x: 6.0, y: 48.0 });
        assert_eq!(p["SW4"], Point { x: 18.0, y: 48.0 });
    }

    #[test]
    fn an_explicit_control_overrides_the_pattern_that_made_it() {
        let f = PlacementFile::from_toml(
            r#"
[controls]
J2 = { x = 20.0, y = 99.0 }

[[patterns.column]]
refdes = ["J1", "J2"]
x      = 6.0
from_y = 10.0
pitch  = 10.0
"#,
        )
        .unwrap();
        let p = f.positions().unwrap();
        assert_eq!(p["J1"], Point { x: 6.0, y: 10.0 });
        // Nudging one part out of a strip must not require unpicking the strip.
        assert_eq!(p["J2"], Point { x: 20.0, y: 99.0 });
    }

    /// Two patterns disagreeing is a mistake in the file. Picking one silently
    /// puts a hole in the panel that the board will not meet.
    #[test]
    fn the_same_part_placed_by_two_patterns_is_an_error() {
        let err = PlacementFile::from_toml(
            r#"
[[patterns.column]]
refdes = ["J1"]
x = 6.0
from_y = 10.0
pitch = 10.0

[[patterns.row]]
refdes = ["J1"]
y = 50.0
from_x = 6.0
pitch = 10.0
"#,
        )
        .unwrap()
        .positions()
        .unwrap_err();
        assert!(matches!(err, PlacementError::Duplicate(r) if r == "J1"));
    }

    #[test]
    fn a_zero_column_grid_is_rejected_rather_than_dividing_by_zero() {
        let err = PlacementFile::from_toml(
            r#"
[[patterns.grid]]
refdes = ["A"]
x = 0.0
y = 0.0
cols = 0
pitch_x = 1.0
pitch_y = 1.0
"#,
        )
        .unwrap()
        .positions()
        .unwrap_err();
        assert!(matches!(err, PlacementError::EmptyGrid(_)));
    }

    /// The flip is what keeps the hand-authored file, the board and the derived
    /// panel describing one layout.
    #[test]
    fn anchors_flip_panel_space_into_the_boards_frame() {
        let f = PlacementFile::from_toml(FILE).unwrap();
        let a = f.anchors(128.5).unwrap();
        // A jack 10.5mm up from the bottom is 118mm down from the top.
        assert!((a["J1"].1 - 118.0).abs() < 1e-9, "{:?}", a["J1"]);
        assert!((a["J1"].0 - 6.0).abs() < 1e-9);
        // A pot near the top of the panel is near the top of the board frame.
        assert!(a["RV1"].1 < a["J1"].1);
    }

    #[test]
    fn an_empty_file_is_valid_and_places_nothing() {
        let f = PlacementFile::from_toml("").unwrap();
        assert!(f.positions().unwrap().is_empty());
    }

    mod view {
        use super::*;
        use crate::model::{Circuit, Part};
        use crate::panel::{panel_from_board, BuiltinCutouts, ControlKind};

        /// A 5 HP board: a jack near the bottom, a pot near the top, and one 0603
        /// that is nobody's business but the placer's.
        const BOARD: &str = r#"(kicad_pcb
          (gr_rect (start 100 40) (end 125.4 168.5) (layer "Edge.Cuts"))
          (footprint "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical" (layer "F.Cu") (at 106 158 0)
            (property "Reference" "J1") (pad "1" thru_hole circle (at 0 0) (size 2 2)))
          (footprint "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical" (layer "F.Cu") (at 118 55 0)
            (property "Reference" "RV1") (pad "1" thru_hole circle (at 0 0) (size 2 2)))
          (footprint "Resistor_SMD:R_0603_1608Metric" (layer "F.Cu") (at 110 100 0)
            (property "Reference" "R1") (pad "1" smd rect (at 0 0) (size 1 1))))"#;

        fn circuit() -> Circuit {
            Circuit {
                name: "t".into(),
                parts: vec![
                    Part::new("J1", "AudioJack2_SwitchT")
                        .with_footprint("Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical"),
                    Part::new("RV1", "100k").with_footprint(
                        "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical",
                    ),
                    Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0603_1608Metric"),
                ],
                nets: vec![],
            }
        }

        fn view() -> BoardView {
            board_view(BOARD, &circuit(), &BuiltinCutouts).expect("view")
        }

        #[test]
        fn reads_the_board_into_panel_space() {
            let v = view();
            assert_eq!(v.hp, 5);
            assert!((v.width_mm - 25.4).abs() < 1e-9);
            assert!((v.height_mm - 128.5).abs() < 1e-9);
            // Board internals are present, not filtered out — the editor draws
            // them dimmed so a jack is not placed on top of a resistor.
            assert_eq!(v.parts.len(), 3);
            assert_eq!(v.controls().count(), 2);
            let r1 = v.parts.iter().find(|p| p.refdes == "R1").unwrap();
            assert!(r1.cutout.is_none(), "an 0603 is not panel hardware");
        }

        /// The editor and the derived panel must agree to the micron, or a knob
        /// drawn where the editor thinks it is gets a hole somewhere else.
        #[test]
        fn agrees_with_the_panel_derived_from_the_same_board() {
            let v = view();
            let p = panel_from_board(BOARD, &circuit(), &BuiltinCutouts).unwrap();
            assert_eq!(p.cutouts.len(), v.controls().count());
            for c in &p.cutouts {
                let refdes = c.refdes.as_deref().unwrap();
                let got = v.parts.iter().find(|q| q.refdes == refdes).unwrap();
                assert!((got.point.x - c.x_mm).abs() < 1e-9, "{refdes} x");
                assert!((got.point.y - c.y_mm).abs() < 1e-9, "{refdes} y");
            }
        }

        /// The round trip that keeps the file, the board and the panel describing
        /// one layout: a position read off the board, written to a placement file
        /// and flipped back to a board anchor, lands where it started.
        #[test]
        fn a_position_read_off_the_board_flips_back_to_the_same_anchor() {
            let v = view();
            let j1 = v.parts.iter().find(|p| p.refdes == "J1").unwrap();
            let mut file = PlacementFile::default();
            file.controls.insert("J1".into(), j1.point);
            let anchors = file.anchors(v.height_mm).unwrap();
            // Board-local: 6mm across, 118mm down from the top edge.
            assert!((anchors["J1"].0 - 6.0).abs() < 1e-9);
            assert!((anchors["J1"].1 - 118.0).abs() < 1e-9);
        }

        #[test]
        fn carries_the_cutout_envelope_the_editor_needs_for_clearance() {
            let v = view();
            let rv = v.parts.iter().find(|p| p.refdes == "RV1").unwrap();
            let spec = rv.cutout.as_ref().expect("a pot is panel hardware");
            assert_eq!(spec.kind, ControlKind::Pot);
            // A knob is bigger than its hole — that gap is the whole reason the
            // editor needs the envelope and not just the shape.
            assert!(spec.envelope_mm.0 > 7.0, "{:?}", spec.envelope_mm);
        }

        #[test]
        fn a_board_without_an_outline_is_an_error_not_a_guess() {
            let err = board_view("(kicad_pcb)", &circuit(), &BuiltinCutouts).unwrap_err();
            assert!(err.contains("Edge.Cuts"), "{err}");
        }
    }
}
