//! A board's **frame**: the outline it must have and the parts pinned on it —
//! what a form factor turns into once decided (legion-of-bom-3wbu).
//!
//! `lob schematic` writes it beside the circuit as `<stem>.frame.toml`;
//! `lob board` reads it back. With no fixed outline the board is sized to its
//! parts, as before, but with the pinned parts in place at every trial size —
//! so a mounting hole in each corner moves with the corner.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::board::PartFacts;

/// A board's outline and pinned parts, in board-local mm: origin at the top
/// left, x right, y down (KiCad's sense).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardFrame {
    /// The fixed outline; `None` sizes the board to its parts.
    #[serde(default)]
    pub outline: Option<Size>,
    /// Parts pinned at a point: where the part's keep-out is centred.
    #[serde(default)]
    pub pinned: BTreeMap<String, Point>,
    /// Parts pinned in the corners — top left, top right, bottom left, bottom
    /// right, in that order — each inset just far enough that its keep-out
    /// (a screw head's) stays on the board.
    #[serde(default)]
    pub corners: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub width_mm: f64,
    pub height_mm: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x_mm: f64,
    pub y_mm: f64,
}

/// A frame file that could not be used.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame pins {0}, which the circuit does not have")]
    UnknownPart(String),
    #[error("frame puts {0} parts in the corners; a board has 4")]
    TooManyCorners(usize),
}

impl BoardFrame {
    pub fn from_toml(s: &str) -> Result<BoardFrame, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Every part the frame pins must be on the board, and at most four
    /// corners — checked before any layout work, so a stale frame fails loud.
    pub fn check(&self, facts: &HashMap<String, PartFacts>) -> Result<(), FrameError> {
        if self.corners.len() > 4 {
            return Err(FrameError::TooManyCorners(self.corners.len()));
        }
        for r in self.pinned.keys().chain(&self.corners) {
            if !facts.contains_key(r) {
                return Err(FrameError::UnknownPart(r.clone()));
            }
        }
        Ok(())
    }

    /// Where each pinned part's keep-out centre sits on a `w × h` board.
    pub fn anchors(
        &self,
        w: f64,
        h: f64,
        facts: &HashMap<String, PartFacts>,
    ) -> HashMap<String, (f64, f64)> {
        let mut out: HashMap<String, (f64, f64)> = self
            .pinned
            .iter()
            .map(|(r, p)| (r.clone(), (p.x_mm, p.y_mm)))
            .collect();
        for (i, r) in self.corners.iter().enumerate() {
            let Some(f) = facts.get(r) else { continue };
            let (ix, iy) = (f.extent.0 / 2.0, f.extent.1 / 2.0);
            let x = if i % 2 == 0 { ix } else { w - ix };
            let y = if i < 2 { iy } else { h - iy };
            out.insert(r.clone(), (x, y));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_through_toml() {
        let frame = BoardFrame {
            outline: Some(Size {
                width_mm: 65.0,
                height_mm: 56.5,
            }),
            pinned: [(
                "J1".to_string(),
                Point {
                    x_mm: 32.5,
                    y_mm: 3.5,
                },
            )]
            .into(),
            corners: vec!["H1".into(), "H2".into()],
        };
        let back = BoardFrame::from_toml(&frame.to_toml().unwrap()).unwrap();
        assert_eq!(back, frame);
        assert!(
            BoardFrame::from_toml("outlines = 1").is_err(),
            "a typo is an error"
        );
    }
}
