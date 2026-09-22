//! Guitar Pedal `PanelSpec` implementation. DESIGN.md §6.9, §7.7.
//!
//! The second-ever [`PanelSpec`] implementation (after [`crate::panel::EurorackPanel`])
//! — proof that the trait really is the format-agnostic seam DESIGN.md §6.9
//! claims it to be. No HP, no rails: dimensions come from a standard
//! die-cast stompbox enclosure size class ([`crate::spec::EnclosureSize`]),
//! and hardware gets pedal-appropriate hole sizes via [`PedalCutouts`]
//! (Eurorack's [`crate::panel::BuiltinCutouts`] assumes 3.5mm jacks and 9mm
//! pot bushings — wrong for 1/4" jacks and 16mm pedal pots).
//!
//! **Front panel only.** This models the enclosure's front face — the same
//! single-face model `EurorackPanel` uses for a module's front panel. A real
//! stompbox's DC jack is commonly mounted on the top or side edge rather than
//! the front face; representing that needs a multi-face enclosure model this
//! doesn't attempt yet, so the DC jack is intentionally left off this panel
//! rather than forced into a front-face position that would misrepresent the
//! real build. Flagged here, not silently dropped.
//!
//! **First-pass hole sizes**, same status as `BuiltinCutouts`'s table
//! (DESIGN.md §7.7 already names the enclosure set this reads): common
//! values for the hardware families named in each constant's comment, not
//! measured against a specific vendor's drawing. Verify before cutting metal,
//! same caveat `TOGGLE_MM` already carries for Eurorack.

use crate::panel::{
    ControlKind, Cutout, CutoutRole, CutoutShape, CutoutSource, CutoutSpec, MountingHole,
    PanelFormat, PanelSpec,
};
use crate::spec::EnclosureSize;

/// 1/4" mono jack (e.g. Switchcraft #11-style panel-mount): standard 3/8"
/// mounting thread.
const JACK_HOLE_MM: f64 = 9.5;
const JACK_ENVELOPE: (f64, f64) = (20.0, 20.0);
/// 16mm-body pedal pot (e.g. Alpha 16mm): standard 7mm threaded bushing —
/// distinct from Eurorack's 9mm-body/7mm-bushing coincidence in `BuiltinCutouts`.
const POT_HOLE_MM: f64 = 7.0;
const POT_ENVELOPE: (f64, f64) = (18.0, 18.0);
/// Classic 3PDT footswitch (Alpha/Taiway/Carling-style): standard 12mm bushing.
const FOOTSWITCH_HOLE_MM: f64 = 12.0;
const FOOTSWITCH_ENVELOPE: (f64, f64) = (20.0, 20.0);
/// Bare 5mm LED, no bezel.
const LED_HOLE_MM: f64 = 5.0;
const LED_ENVELOPE: (f64, f64) = (6.0, 6.0);

/// Minimum clearance between adjacent control envelopes (larger than
/// Eurorack's 2mm — pedal hardware runs bigger).
const GAP_MM: f64 = 3.0;
/// Panel material left between a control envelope and the panel edge.
const EDGE_MM: f64 = 1.0;

/// Pedal-appropriate hardware cutout catalogue — the [`CutoutSource`] a
/// Guitar Pedal panel resolves against instead of Eurorack's `BuiltinCutouts`.
/// Same matching strategy (MPN first, footprint keyword fallback), different
/// table.
#[derive(Debug, Clone, Copy, Default)]
pub struct PedalCutouts;

impl CutoutSource for PedalCutouts {
    fn cutout(&self, _mpn: Option<&str>, footprint: &str) -> Option<CutoutSpec> {
        let name = footprint
            .rsplit_once(':')
            .map(|(_, r)| r)
            .unwrap_or(footprint)
            .to_ascii_lowercase();
        let circle = |kind: ControlKind, diameter_mm: f64, envelope_mm: (f64, f64)| {
            Some(CutoutSpec {
                kind,
                shape: CutoutShape::Circle { diameter_mm },
                envelope_mm,
            })
        };
        if name.contains("led") {
            circle(ControlKind::Led, LED_HOLE_MM, LED_ENVELOPE)
        } else if ["switch", "toggle", "footswitch", "3pdt"]
            .iter()
            .any(|k| name.contains(k))
        {
            circle(ControlKind::Switch, FOOTSWITCH_HOLE_MM, FOOTSWITCH_ENVELOPE)
        } else if ["potentiometer", "_pot", "alpha16mm"]
            .iter()
            .any(|k| name.contains(k))
        {
            circle(ControlKind::Pot, POT_HOLE_MM, POT_ENVELOPE)
        } else if ["jack", "phone_jack", "6.35mm", "ts_jack"]
            .iter()
            .any(|k| name.contains(k))
        {
            circle(ControlKind::Jack, JACK_HOLE_MM, JACK_ENVELOPE)
        } else {
            None
        }
    }
}

/// A guitar pedal front panel.
///
/// Constructed from an [`EnclosureSize`] class; exposes only mm through the
/// [`PanelSpec`] trait, same as `EurorackPanel` exposes only mm despite being
/// built in HP internally.
#[derive(Debug, Clone, PartialEq)]
pub struct PedalPanel {
    size: EnclosureSize,
    thickness_mm: f64,
    cutouts: Vec<Cutout>,
}

/// Front-face `(width, height)` mm for each enclosure size class, from the
/// vendor's own length/width/depth figures (DESIGN.md §7.7's named set) —
/// the enclosure's *length* runs vertically in the standard stompbox
/// orientation (footswitch at the bottom), so panel height = length, panel
/// width = width.
fn face_mm(size: EnclosureSize) -> (f64, f64) {
    match size {
        EnclosureSize::Size1590B => (60.0, 112.0),
        EnclosureSize::Size1590BB => (94.0, 120.0),
        EnclosureSize::Size125B => (66.0, 125.0),
    }
}

/// `count` envelopes of width `envelope_w`, each `GAP_MM` apart, centered in
/// `available_w` at height `y_mm` — the x-centers, left to right. Generalizes
/// across enclosure widths instead of hand-picking coordinates per size class.
fn centered_row(available_w: f64, count: usize, envelope_w: f64) -> Vec<f64> {
    let n = count as f64;
    let total = n * envelope_w + (n - 1.0).max(0.0) * GAP_MM;
    let start = (available_w - total) / 2.0;
    (0..count)
        .map(|i| start + i as f64 * (envelope_w + GAP_MM) + envelope_w / 2.0)
        .collect()
}

impl PedalPanel {
    /// An empty pedal panel of the given size — no cutouts. The base a
    /// `PanelFile`'s own `cutouts` list gets layered onto via
    /// [`with_cutout_spec`](Self::with_cutout_spec), the same shape
    /// `EurorackPanel::with_format` + its builder methods already use. This
    /// is what [`crate::panel::PanelFile::to_spec`] actually builds on for a
    /// Guitar Pedal format — the cutout *positions* are the panel file's
    /// data, same as Eurorack, not baked into this struct.
    pub fn empty(size: EnclosureSize) -> Self {
        PedalPanel {
            size,
            thickness_mm: 1.6,
            cutouts: Vec::new(),
        }
    }

    /// Add one cutout (mirrors `EurorackPanel::with_cutout_spec`).
    pub fn with_cutout_spec(mut self, cutout: Cutout) -> Self {
        self.cutouts.push(cutout);
        self
    }

    /// A pedal panel of the given enclosure size, laid out for the standard
    /// controls a fuzz-family circuit needs: 2 jacks (top), 2 pots (middle),
    /// an LED and footswitch (bottom). Every position is computed from the
    /// enclosure's own width/height, not hand-picked per size class.
    ///
    /// `refdes` names the board parts to anchor at the pot positions (e.g.
    /// `("RV1", "RV2")` for a fuzz-pedal spec's Fuzz/Volume pots) — `None`
    /// leaves a position unanchored (panel geometry only, no board part
    /// placed there), matching how jacks/footswitch/LED are wired via loose
    /// leads rather than PCB-mounted in this design.
    pub fn fuzz_pedal(size: EnclosureSize, pot_refdes: (&str, &str)) -> Self {
        let (w, h) = face_mm(size);
        let thickness_mm = 1.6; // typical die-cast aluminum lid thickness

        let jack_y = h - EDGE_MM - JACK_ENVELOPE.1 / 2.0;
        let pot_y = h * 0.55;
        let footswitch_y = EDGE_MM + FOOTSWITCH_ENVELOPE.1 / 2.0 + 4.0;
        let led_y = footswitch_y + FOOTSWITCH_ENVELOPE.1 / 2.0 + GAP_MM + LED_ENVELOPE.1 / 2.0;

        let jack_x = centered_row(w, 2, JACK_ENVELOPE.0);
        let pot_x = centered_row(w, 2, POT_ENVELOPE.0);

        let cutouts = vec![
            Cutout {
                x_mm: jack_x[0],
                y_mm: jack_y,
                rotation_deg: 0.0,
                footprint: "Jack_6.35mm_TS".to_string(),
                refdes: None,
                label: Some("IN".to_string()),
                role: Some(CutoutRole::Io),
            },
            Cutout {
                x_mm: jack_x[1],
                y_mm: jack_y,
                rotation_deg: 0.0,
                footprint: "Jack_6.35mm_TS".to_string(),
                refdes: None,
                label: Some("OUT".to_string()),
                role: Some(CutoutRole::Io),
            },
            Cutout {
                x_mm: pot_x[0],
                y_mm: pot_y,
                rotation_deg: 0.0,
                footprint: "Potentiometer_16mm".to_string(),
                refdes: Some(pot_refdes.0.to_string()),
                label: Some("FUZZ".to_string()),
                role: Some(CutoutRole::Knob),
            },
            Cutout {
                x_mm: pot_x[1],
                y_mm: pot_y,
                rotation_deg: 0.0,
                footprint: "Potentiometer_16mm".to_string(),
                refdes: Some(pot_refdes.1.to_string()),
                label: Some("VOLUME".to_string()),
                role: Some(CutoutRole::Knob),
            },
            Cutout {
                x_mm: w / 2.0,
                y_mm: led_y,
                rotation_deg: 0.0,
                footprint: "LED_5mm".to_string(),
                refdes: None,
                label: None,
                role: None,
            },
            Cutout {
                x_mm: w / 2.0,
                y_mm: footswitch_y,
                rotation_deg: 0.0,
                footprint: "Footswitch_3PDT".to_string(),
                refdes: None,
                label: None,
                role: Some(CutoutRole::Switch),
            },
        ];

        PedalPanel {
            size,
            thickness_mm,
            cutouts,
        }
    }

    /// Override the default thickness (mm).
    pub fn with_thickness(mut self, mm: f64) -> Self {
        self.thickness_mm = mm;
        self
    }

    pub fn size(&self) -> EnclosureSize {
        self.size
    }
}

impl PanelSpec for PedalPanel {
    fn width_mm(&self) -> f64 {
        face_mm(self.size).0
    }

    fn height_mm(&self) -> f64 {
        face_mm(self.size).1
    }

    fn thickness_mm(&self) -> f64 {
        self.thickness_mm
    }

    fn mounting_holes(&self) -> &[MountingHole] {
        // A die-cast stompbox lid is held by side screws through the
        // enclosure's lip, not holes through the front face — correctly
        // empty, not an oversight (contrast EurorackPanel, which is
        // rail-mounted through its own front face).
        &[]
    }

    fn cutouts(&self) -> &[Cutout] {
        &self.cutouts
    }

    fn format(&self) -> PanelFormat {
        PanelFormat::Eurorack3U // title-placement default: banner above controls, same as a 3U panel
    }

    fn cutout_source(&self) -> &dyn CutoutSource {
        &PedalCutouts
    }
}

/// A Guitar Pedal panel format token in a `PanelFile` (`format = "pedal-1590b"`
/// etc.) — the counterpart Eurorack panels write as `"eurorack"`/`"1u"`/etc.
pub fn pedal_format_token(size: EnclosureSize) -> String {
    format!("pedal-{}", size.key().to_ascii_lowercase())
}

/// Render [`PedalPanel::fuzz_pedal`]'s layout as an actual, editable
/// [`crate::panel::PanelFile`] — the file `lob panel derive` (or a one-off
/// script) writes so `lob board --panel <that file>` has real `[[cutouts]]`
/// entries to check the circuit against, the same way a Eurorack panel's
/// cutouts live in its own TOML rather than being reconstructed from a format
/// string at every `to_spec()` call.
pub fn fuzz_pedal_panel_file(
    size: EnclosureSize,
    pot_refdes: (&str, &str),
    thickness_mm: f64,
) -> crate::panel::PanelFile {
    let panel = PedalPanel::fuzz_pedal(size, pot_refdes);
    let cutouts = panel
        .cutouts()
        .iter()
        .map(|c| crate::panel::CutoutFile {
            x_mm: c.x_mm,
            y_mm: c.y_mm,
            rotation_deg: c.rotation_deg,
            footprint: c.footprint.clone(),
            refdes: c.refdes.clone(),
            label: c.label.clone(),
            role: c.role.map(|r| r.as_str().to_string()),
        })
        .collect();
    crate::panel::PanelFile {
        format: pedal_format_token(size),
        hp: None,
        thickness_mm,
        finish: None,
        cutouts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pedal_cutouts_matches_pedal_hardware_not_eurorack_sizes() {
        let jack = PedalCutouts.cutout(None, "Jack_6.35mm_TS").unwrap();
        assert_eq!(jack.kind, ControlKind::Jack);
        assert_eq!(
            jack.shape,
            CutoutShape::Circle {
                diameter_mm: JACK_HOLE_MM
            }
        );

        let pot = PedalCutouts.cutout(None, "Potentiometer_16mm").unwrap();
        assert_eq!(pot.kind, ControlKind::Pot);
        assert_eq!(
            pot.shape,
            CutoutShape::Circle {
                diameter_mm: POT_HOLE_MM
            }
        );

        let sw = PedalCutouts.cutout(None, "Footswitch_3PDT").unwrap();
        assert_eq!(sw.kind, ControlKind::Switch);

        assert!(PedalCutouts
            .cutout(None, "Resistor_SMD:R_0805_2012Metric")
            .is_none());
    }

    #[test]
    fn centered_row_stays_within_available_width() {
        for &(w, count, envelope) in &[(60.0, 2, 20.0), (94.0, 2, 20.0), (66.0, 2, 18.0)] {
            let xs = centered_row(w, count, envelope);
            for &x in &xs {
                assert!(x - envelope / 2.0 >= 0.0, "x={x} envelope={envelope} w={w}");
                assert!(x + envelope / 2.0 <= w, "x={x} envelope={envelope} w={w}");
            }
        }
    }

    #[test]
    fn fuzz_pedal_panel_has_no_overlapping_cutouts_at_every_enclosure_size() {
        for size in [
            EnclosureSize::Size1590B,
            EnclosureSize::Size1590BB,
            EnclosureSize::Size125B,
        ] {
            let panel = PedalPanel::fuzz_pedal(size, ("RV1", "RV2"));
            let cutouts = panel.cutouts();
            for (i, a) in cutouts.iter().enumerate() {
                for b in &cutouts[i + 1..] {
                    let dx = (a.x_mm - b.x_mm).abs();
                    let dy = (a.y_mm - b.y_mm).abs();
                    let dist = (dx * dx + dy * dy).sqrt();
                    // Envelopes are ~18-20mm; any two centers closer than 15mm
                    // apart would visibly overlap regardless of exact shape.
                    assert!(
                        dist > 15.0,
                        "{size:?}: cutouts too close: {:?} vs {:?} (dist {dist:.1}mm)",
                        a.footprint,
                        b.footprint
                    );
                }
            }
        }
    }

    #[test]
    fn fuzz_pedal_panel_anchors_the_named_pots_and_leaves_hardware_unanchored() {
        let panel = PedalPanel::fuzz_pedal(EnclosureSize::Size1590B, ("RV1", "RV2"));
        let anchored: Vec<&str> = panel
            .cutouts()
            .iter()
            .filter_map(|c| c.refdes.as_deref())
            .collect();
        assert_eq!(anchored, vec!["RV1", "RV2"]);
        assert!(panel.mounting_holes().is_empty());
    }

    #[test]
    fn dxf_render_uses_pedal_hole_sizes_not_eurorack_defaults() {
        // Regression check for the PanelSpec::cutout_source seam: before it
        // existed, DXF/PCB rendering called the free footprint_shape()
        // function, which is hardcoded to Eurorack's BuiltinCutouts -- a
        // pedal panel would have silently rendered 6mm/4.95mm Eurorack jack
        // and toggle holes instead of the 9.5mm/12mm pedal ones.
        let panel = PedalPanel::fuzz_pedal(EnclosureSize::Size1590B, ("RV1", "RV2"));
        let dxf = crate::panel::panel_to_dxf(&panel);
        assert!(
            dxf.contains("\n4.75\n"),
            "expected the 9.5mm jack hole's radius (4.75) in the DXF"
        );
        assert!(
            dxf.contains("\n6\n"),
            "expected the 12mm footswitch hole's radius (6) in the DXF"
        );
    }

    #[test]
    fn dimensions_match_named_enclosure_face_size() {
        let panel = PedalPanel::fuzz_pedal(EnclosureSize::Size1590B, ("RV1", "RV2"));
        assert_eq!(panel.width_mm(), 60.0);
        assert_eq!(panel.height_mm(), 112.0);
    }
}
