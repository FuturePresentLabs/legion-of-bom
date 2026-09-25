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
    /// (a screw head's) keeps the board's edge clearance
    /// ([`crate::rules::EDGE_CLEARANCE_MM`]), which a pinned part cannot be
    /// moved to satisfy.
    #[serde(default)]
    pub corners: Vec<String>,
    /// Relational placement requirements selected by the design/profile.
    #[serde(default)]
    pub placement: crate::rules::PlacementIntent,
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

/// Deterministic output of the corner-standoff board tool.
///
/// The footprint remains the source of truth for the physical hole and its
/// courtyard. This tool turns those measured facts into board-relative
/// placement and keep-out geometry; callers do not guess an M3 inset again.
#[derive(Debug, Clone, PartialEq)]
pub struct StandoffPattern {
    pub holes: Vec<StandoffHole>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StandoffHole {
    pub reference: String,
    pub center: Point,
    /// Axis-aligned footprint/courtyard keep-out on the finished board.
    pub keepout: Keepout,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keepout {
    pub min_x_mm: f64,
    pub min_y_mm: f64,
    pub max_x_mm: f64,
    pub max_y_mm: f64,
}

/// A frame file that could not be used.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame pins {0}, which the circuit does not have")]
    UnknownPart(String),
    #[error("invalid placement intent: {0}")]
    InvalidPlacement(String),
    #[error("frame puts {0} parts in the corners; a board has 4")]
    TooManyCorners(usize),
    #[error("board outline must be positive, got {width_mm} × {height_mm} mm")]
    InvalidOutline { width_mm: f64, height_mm: f64 },
    #[error(
        "{width_mm} × {height_mm} mm board is too small for corner standoffs; their measured keep-outs require at least {required_width_mm} × {required_height_mm} mm"
    )]
    StandoffsDoNotFit {
        width_mm: f64,
        height_mm: f64,
        required_width_mm: f64,
        required_height_mm: f64,
    },
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
        self.placement
            .validate()
            .map_err(FrameError::InvalidPlacement)?;
        if self.corners.len() > 4 {
            return Err(FrameError::TooManyCorners(self.corners.len()));
        }
        for r in self.pinned.keys().chain(&self.corners) {
            if !facts.contains_key(r) {
                return Err(FrameError::UnknownPart(r.clone()));
            }
        }
        for reference in self
            .placement
            .orientations
            .iter()
            .map(|orientation| &orientation.refdes)
            .chain(
                self.placement
                    .clusters
                    .iter()
                    .flat_map(|cluster| cluster.members.iter()),
            )
            .chain(
                self.placement
                    .keepouts
                    .iter()
                    .flat_map(|region| region.exempt.iter()),
            )
        {
            if !facts.contains_key(reference) {
                return Err(FrameError::UnknownPart(reference.clone()));
            }
        }
        Ok(())
    }

    /// Run the bounded corner-standoff tool for a `w × h` board.
    ///
    /// Insets and keep-outs come from the actual footprint facts. Two holes on
    /// an edge must have non-overlapping keep-outs; impossible outlines fail
    /// here, before placement or routing.
    pub fn standoff_pattern(
        &self,
        w: f64,
        h: f64,
        facts: &HashMap<String, PartFacts>,
    ) -> Result<StandoffPattern, FrameError> {
        if w <= 0.0 || h <= 0.0 {
            return Err(FrameError::InvalidOutline {
                width_mm: w,
                height_mm: h,
            });
        }
        if self.corners.is_empty() {
            return Ok(StandoffPattern { holes: Vec::new() });
        }

        let edge = crate::rules::EDGE_CLEARANCE_MM;
        let mut left_w = 0.0_f64;
        let mut right_w = 0.0_f64;
        let mut top_h = 0.0_f64;
        let mut bottom_h = 0.0_f64;
        for (i, reference) in self.corners.iter().enumerate() {
            let Some(f) = facts.get(reference) else {
                return Err(FrameError::UnknownPart(reference.clone()));
            };
            if i % 2 == 0 {
                left_w = left_w.max(f.extent.0);
            } else {
                right_w = right_w.max(f.extent.0);
            }
            if i < 2 {
                top_h = top_h.max(f.extent.1);
            } else {
                bottom_h = bottom_h.max(f.extent.1);
            }
        }
        let required_w = 2.0 * edge + left_w + right_w;
        let required_h = 2.0 * edge + top_h + bottom_h;
        if w < required_w || h < required_h {
            return Err(FrameError::StandoffsDoNotFit {
                width_mm: w,
                height_mm: h,
                required_width_mm: required_w,
                required_height_mm: required_h,
            });
        }

        let mut holes = Vec::with_capacity(self.corners.len());
        for (i, reference) in self.corners.iter().enumerate() {
            let Some(f) = facts.get(reference) else {
                return Err(FrameError::UnknownPart(reference.clone()));
            };
            let (fw, fh) = f.extent;
            let ix = fw / 2.0 + edge;
            let iy = fh / 2.0 + edge;
            let x = if i % 2 == 0 { ix } else { w - ix };
            let y = if i < 2 { iy } else { h - iy };
            holes.push(StandoffHole {
                reference: reference.clone(),
                center: Point { x_mm: x, y_mm: y },
                keepout: Keepout {
                    min_x_mm: x - fw / 2.0,
                    min_y_mm: y - fh / 2.0,
                    max_x_mm: x + fw / 2.0,
                    max_y_mm: y + fh / 2.0,
                },
            });
        }
        Ok(StandoffPattern { holes })
    }

    /// Where every pinned part's keep-out centre sits on a `w × h` board.
    pub fn anchors(
        &self,
        w: f64,
        h: f64,
        facts: &HashMap<String, PartFacts>,
    ) -> Result<HashMap<String, (f64, f64)>, FrameError> {
        let mut out: HashMap<String, (f64, f64)> = self
            .pinned
            .iter()
            .map(|(r, p)| (r.clone(), (p.x_mm, p.y_mm)))
            .collect();
        for hole in self.standoff_pattern(w, h, facts)?.holes {
            out.insert(hole.reference, (hole.center.x_mm, hole.center.y_mm));
        }
        Ok(out)
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
            placement: crate::rules::PlacementIntent::default(),
        };
        let back = BoardFrame::from_toml(&frame.to_toml().unwrap()).unwrap();
        assert_eq!(back, frame);
        assert!(
            BoardFrame::from_toml("outlines = 1").is_err(),
            "a typo is an error"
        );
    }

    #[test]
    fn corner_parts_keep_the_edge_clearance_at_any_size() {
        use crate::board::PartFacts;
        use crate::model::Side;
        let hole = PartFacts {
            extent: (6.9, 6.9),
            body_extent: (6.9, 6.9),
            origin_offset: (0.0, 0.0),
            side: Side::Front,
            height_mm: 0.0,
            standoff_mm: None,
            tht_pads: Vec::new(),
            pin_offsets: HashMap::new(),
        };
        let facts: HashMap<String, PartFacts> = ["H1", "H2", "H3", "H4"]
            .iter()
            .map(|r| (r.to_string(), hole.clone()))
            .collect();
        let frame = BoardFrame {
            corners: vec!["H1".into(), "H2".into(), "H3".into(), "H4".into()],
            ..BoardFrame::default()
        };
        let inset = 6.9 / 2.0 + crate::rules::EDGE_CLEARANCE_MM;
        for (w, h) in [(30.0, 30.0), (50.0, 40.0)] {
            let a = frame.anchors(w, h, &facts).unwrap();
            assert_eq!(a["H1"], (inset, inset));
            assert_eq!(a["H2"], (w - inset, inset));
            assert_eq!(a["H3"], (inset, h - inset));
            assert_eq!(a["H4"], (w - inset, h - inset));
        }
        assert!(frame.check(&facts).is_ok());
        let missing = BoardFrame {
            corners: vec!["H9".into()],
            ..BoardFrame::default()
        };
        assert!(matches!(missing.check(&facts), Err(FrameError::UnknownPart(r)) if r == "H9"));
    }

    #[test]
    fn standoff_tool_exposes_keepouts_and_rejects_an_impossible_board() {
        use crate::model::Side;
        let hole = PartFacts {
            extent: (6.9, 6.9),
            body_extent: (6.9, 6.9),
            origin_offset: (0.0, 0.0),
            side: Side::Front,
            height_mm: 0.0,
            standoff_mm: None,
            tht_pads: Vec::new(),
            pin_offsets: HashMap::new(),
        };
        let facts = ["H1", "H2", "H3", "H4"]
            .into_iter()
            .map(|r| (r.to_string(), hole.clone()))
            .collect();
        let frame = BoardFrame {
            corners: vec!["H1".into(), "H2".into(), "H3".into(), "H4".into()],
            ..BoardFrame::default()
        };

        let pattern = frame.standoff_pattern(40.0, 30.0, &facts).unwrap();
        assert_eq!(pattern.holes.len(), 4);
        let first = &pattern.holes[0];
        assert_eq!(first.reference, "H1");
        assert_eq!(first.keepout.min_x_mm, crate::rules::EDGE_CLEARANCE_MM);
        assert_eq!(first.keepout.min_y_mm, crate::rules::EDGE_CLEARANCE_MM);
        assert!(matches!(
            frame.standoff_pattern(16.0, 30.0, &facts),
            Err(FrameError::StandoffsDoNotFit { .. })
        ));
    }
}
