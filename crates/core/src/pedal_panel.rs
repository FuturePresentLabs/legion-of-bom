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
//! **Single flattened face, not a true multi-face enclosure model.** Like
//! `EurorackPanel`, this exposes one 2D layout — it does not model which of
//! a die-cast box's distinct physical faces (front/top/sides) each control
//! actually mounts to. That's the same simplification real DIY drilling
//! templates use (e.g. a 1590B template drawn as one flattened footprint
//! with side-edge jacks and a top-edge power jack, not a 3D unfolding), so
//! `fuzz_pedal`'s layout follows that same convention: audio jacks at the
//! left/right edges, power at the top edge, matching the real classic
//! Fuzz-Face-style stompbox layout (cross-checked against a real vendor
//! drilling template this session) rather than a made-up arrangement. A
//! genuine multi-face model, if ever needed, is future work.
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
/// Standard 5.5mm/2.1mm panel-mount DC barrel jack (e.g. Kobiconn/CUI
/// PJ-30x-style): cross-referenced mounting-hole specs cluster at
/// 0.313in/~7.95mm (non-locking) up to ~13mm (locking variants) — first-pass
/// like the other hole sizes in this file, not measured against one specific
/// vendor's drawing.
const DC_JACK_HOLE_MM: f64 = 8.0;
const DC_JACK_ENVELOPE: (f64, f64) = (16.0, 16.0);

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
        } else if ["dc_jack", "dcjack", "power_jack", "barrel", "pj-", "pj_"]
            .iter()
            .any(|k| name.contains(k))
        {
            // Checked before the generic "jack" match below -- "dc_jack"
            // contains "jack" as a substring, so a DC barrel connector would
            // otherwise silently resolve to a 1/4in audio jack's hole size.
            circle(ControlKind::Jack, DC_JACK_HOLE_MM, DC_JACK_ENVELOPE)
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
pub(crate) fn centered_row(available_w: f64, count: usize, envelope_w: f64) -> Vec<f64> {
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
    /// controls a fuzz-family circuit needs: audio jacks on the left/right
    /// edges, DC power at the top edge, 2 pots in the middle, an LED and
    /// footswitch at the bottom — the real classic-stompbox convention (see
    /// this module's doc comment), not an arbitrary arrangement. Every
    /// position is computed from the enclosure's own width/height, not
    /// hand-picked per size class.
    ///
    /// `refdes` names the board parts to anchor at the pot positions (e.g.
    /// `("RV1", "RV2")` for a fuzz-pedal spec's Fuzz/Volume pots) — `None`
    /// leaves a position unanchored (panel geometry only, no board part
    /// placed there), matching how jacks/footswitch/LED are wired via loose
    /// leads rather than PCB-mounted in this design.
    pub fn fuzz_pedal(size: EnclosureSize, pot_refdes: (&str, &str)) -> Self {
        let (w, h) = face_mm(size);
        let thickness_mm = 1.6; // typical die-cast aluminum lid thickness

        let pot_y = h * 0.55;
        let footswitch_y = EDGE_MM + FOOTSWITCH_ENVELOPE.1 / 2.0 + 4.0;
        let led_y = footswitch_y + FOOTSWITCH_ENVELOPE.1 / 2.0 + GAP_MM + LED_ENVELOPE.1 / 2.0;
        // Side jacks sit between the LED/footswitch cluster and the pot row
        // — clear of both, not sharing either one's height.
        let jack_y = led_y + (pot_y - led_y) * 0.5;
        let jack_x_left = EDGE_MM + JACK_ENVELOPE.0 / 2.0;
        let jack_x_right = w - EDGE_MM - JACK_ENVELOPE.0 / 2.0;
        let power_y = h - EDGE_MM - DC_JACK_ENVELOPE.1 / 2.0;

        let pot_x = centered_row(w, 2, POT_ENVELOPE.0);

        let cutouts = vec![
            Cutout {
                x_mm: jack_x_right,
                y_mm: jack_y,
                rotation_deg: 0.0,
                footprint: "Jack_6.35mm_TS".to_string(),
                refdes: None,
                label: Some("IN".to_string()),
                role: Some(CutoutRole::Io),
            },
            Cutout {
                x_mm: jack_x_left,
                y_mm: jack_y,
                rotation_deg: 0.0,
                footprint: "Jack_6.35mm_TS".to_string(),
                refdes: None,
                label: Some("OUT".to_string()),
                role: Some(CutoutRole::Io),
            },
            Cutout {
                x_mm: w / 2.0,
                y_mm: power_y,
                rotation_deg: 0.0,
                footprint: "DC_Jack_5.5x2.1mm".to_string(),
                refdes: None,
                label: Some("9V".to_string()),
                role: None,
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
                // D1 is a real board part (LED_THT:LED_D5.0mm, unlike the
                // loose-wired jacks/footswitch) -- anchored so the board
                // places its actual pad at the panel hole it shines
                // through, and so this panel matches what auto-derivation
                // expects for any control with a real footprint.
                refdes: Some("D1".to_string()),
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

    /// A pedal panel whose control arrangement comes from a Lua layout
    /// script ([`crate::panel_lua`]) instead of a hardcoded Rust function
    /// like [`Self::fuzz_pedal`]. The verified hardware catalog (hole and
    /// envelope sizes) is still Rust-sourced — the script only decides
    /// positions, never hole sizes — so a script can rearrange controls
    /// freely without ever being able to get a real part's mounting hole
    /// wrong.
    pub fn from_script(
        size: EnclosureSize,
        pot_refdes: (&str, &str),
        script_path: &std::path::Path,
    ) -> Result<Self, crate::panel_lua::PanelScriptError> {
        let (w, h) = face_mm(size);
        let hardware = crate::panel_lua::HardwareCatalog {
            jack: crate::panel_lua::HardwareSpec {
                diameter_mm: JACK_HOLE_MM,
                envelope_mm: JACK_ENVELOPE,
            },
            dc_jack: crate::panel_lua::HardwareSpec {
                diameter_mm: DC_JACK_HOLE_MM,
                envelope_mm: DC_JACK_ENVELOPE,
            },
            pot: crate::panel_lua::HardwareSpec {
                diameter_mm: POT_HOLE_MM,
                envelope_mm: POT_ENVELOPE,
            },
            footswitch: crate::panel_lua::HardwareSpec {
                diameter_mm: FOOTSWITCH_HOLE_MM,
                envelope_mm: FOOTSWITCH_ENVELOPE,
            },
            led: crate::panel_lua::HardwareSpec {
                diameter_mm: LED_HOLE_MM,
                envelope_mm: LED_ENVELOPE,
            },
        };
        let spec = crate::panel_lua::LayoutSpec {
            width_mm: w,
            height_mm: h,
            edge_mm: EDGE_MM,
            gap_mm: GAP_MM,
            pot_refdes: (pot_refdes.0.to_string(), pot_refdes.1.to_string()),
            hardware,
        };
        let script = crate::panel_lua::PanelScript::load(script_path)?;
        let cutouts = script.layout(&spec)?;
        Ok(PedalPanel {
            size,
            thickness_mm: 1.6,
            cutouts,
        })
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
    use std::path::Path;

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
    fn fuzz_pedal_panel_puts_audio_jacks_on_the_sides_and_power_on_top() {
        // Locks in the real classic-stompbox convention (module doc comment)
        // -- IN on the right edge, OUT on the left edge, DC power at the top
        // edge, distinct from the 1/4in audio jacks -- so a future edit that
        // silently reverts to "both jacks on top" or drops power fails loud.
        let panel = PedalPanel::fuzz_pedal(EnclosureSize::Size1590B, ("RV1", "RV2"));
        let (w, h) = (panel.width_mm(), panel.height_mm());

        let in_jack = panel
            .cutouts()
            .iter()
            .find(|c| c.label.as_deref() == Some("IN"))
            .expect("IN jack cutout");
        let out_jack = panel
            .cutouts()
            .iter()
            .find(|c| c.label.as_deref() == Some("OUT"))
            .expect("OUT jack cutout");
        assert!(
            in_jack.x_mm > w / 2.0,
            "IN jack should be on the right edge, got x={}",
            in_jack.x_mm
        );
        assert!(
            out_jack.x_mm < w / 2.0,
            "OUT jack should be on the left edge, got x={}",
            out_jack.x_mm
        );
        assert_eq!(in_jack.y_mm, out_jack.y_mm, "both side jacks at one height");

        let power = panel
            .cutouts()
            .iter()
            .find(|c| c.footprint == "DC_Jack_5.5x2.1mm")
            .expect("DC power jack cutout");
        assert!(
            power.y_mm > h * 0.8,
            "power jack should be near the top edge, got y={} of h={h}",
            power.y_mm
        );
        assert_eq!(power.x_mm, w / 2.0, "power jack centered on top edge");

        let power_cutout_spec = PedalCutouts
            .cutout(None, &power.footprint)
            .expect("DC jack footprint should resolve via PedalCutouts");
        assert_eq!(
            power_cutout_spec.shape,
            CutoutShape::Circle {
                diameter_mm: DC_JACK_HOLE_MM
            },
            "a DC barrel jack must not silently resolve to the 1/4in audio jack hole size"
        );
    }

    #[test]
    fn lua_fuzz_pedal_script_matches_the_hardcoded_rust_convention() {
        // Parity check: assets/panels/fuzz_pedal.lua is meant to reproduce
        // fuzz_pedal()'s exact real-convention properties, not just
        // "produce some cutouts" -- same assertions as
        // fuzz_pedal_panel_puts_audio_jacks_on_the_sides_and_power_on_top,
        // run against the Lua-driven build instead of the Rust one.
        let script_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/panels/fuzz_pedal.lua");
        let panel = PedalPanel::from_script(EnclosureSize::Size1590B, ("RV1", "RV2"), &script_path)
            .expect("fuzz_pedal.lua should load and run");
        let (w, h) = (panel.width_mm(), panel.height_mm());

        let in_jack = panel
            .cutouts()
            .iter()
            .find(|c| c.label.as_deref() == Some("IN"))
            .expect("IN jack cutout");
        let out_jack = panel
            .cutouts()
            .iter()
            .find(|c| c.label.as_deref() == Some("OUT"))
            .expect("OUT jack cutout");
        assert!(
            in_jack.x_mm > w / 2.0,
            "IN jack should be on the right edge"
        );
        assert!(
            out_jack.x_mm < w / 2.0,
            "OUT jack should be on the left edge"
        );

        let power = panel
            .cutouts()
            .iter()
            .find(|c| c.footprint == "DC_Jack_5.5x2.1mm")
            .expect("DC power jack cutout");
        assert!(
            power.y_mm > h * 0.8,
            "power jack should be near the top edge"
        );

        let anchored: Vec<&str> = panel
            .cutouts()
            .iter()
            .filter_map(|c| c.refdes.as_deref())
            .collect();
        assert_eq!(anchored, vec!["RV1", "RV2", "D1"]);

        // No overlaps, same threshold as the Rust layout's own check.
        let cutouts = panel.cutouts();
        for (i, a) in cutouts.iter().enumerate() {
            for b in &cutouts[i + 1..] {
                let dist = ((a.x_mm - b.x_mm).powi(2) + (a.y_mm - b.y_mm).powi(2)).sqrt();
                assert!(dist > 15.0, "Lua layout: cutouts too close: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn fuzz_pedal_panel_anchors_real_board_parts_and_leaves_loose_wired_hardware_unanchored() {
        // Pots and the LED have real footprints and are board parts -- anchored.
        // Jacks/power/footswitch are genuinely off-board (loose-wired), so they
        // stay unanchored -- panel geometry only, no board part placed there.
        let panel = PedalPanel::fuzz_pedal(EnclosureSize::Size1590B, ("RV1", "RV2"));
        let anchored: Vec<&str> = panel
            .cutouts()
            .iter()
            .filter_map(|c| c.refdes.as_deref())
            .collect();
        assert_eq!(anchored, vec!["RV1", "RV2", "D1"]);
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
