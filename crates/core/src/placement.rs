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
}
