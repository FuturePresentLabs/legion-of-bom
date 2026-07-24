//! Format-preserving edits to a **panel spec** TOML (gka).
//!
//! Sibling to [`crate::edit`], which edits `lob.toml`. The dashboard's panel
//! tuning surface writes back only the builder-owned *mechanical* fields — the
//! panel width (`hp`), the material `finish`, and `thickness_mm` — and NEVER the
//! cutout topology (`[[cutouts]]` positions / `refdes`), which the board placer
//! mates to. That boundary is structural: [`apply_panel_edit_str`] only ever
//! addresses those three top-level keys; no code path here reaches a cutout.
//!
//! Edits go through `toml_edit` so comments, key order, and the cutout tables
//! survive the round-trip, then the result is re-parsed as a [`PanelFile`] so a
//! malformed edit fails loudly instead of corrupting the spec.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml_edit::{value, DocumentMut};

use crate::panel::{PanelFile, PanelFinish};

/// The valid panel-width range in HP — a 1 HP blank up to a full Doepfer row.
const HP_RANGE: std::ops::RangeInclusive<u16> = 1..=84;
/// Sane FR4/aluminium panel thickness range in mm.
const THICKNESS_RANGE: std::ops::RangeInclusive<f64> = 0.5..=5.0;

/// Errors applying a panel-spec edit.
#[derive(Debug, thiserror::Error)]
pub enum PanelEditError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing panel spec: {0}")]
    Parse(#[from] toml_edit::TomlError),
    #[error("edit produced an invalid panel spec: {0}")]
    Invalid(String),
    #[error("hp must be between {} and {} HP", HP_RANGE.start(), HP_RANGE.end())]
    BadHp,
    #[error("unknown finish '{0}' — use a name (black/silver/white/green/blue/red) or #rrggbb")]
    BadFinish(String),
    #[error("thickness must be between {} and {} mm", THICKNESS_RANGE.start(), THICKNESS_RANGE.end())]
    BadThickness,
}

/// A whitelisted set of panel-spec changes. Every field is optional; `None`
/// leaves the current value untouched. An empty `finish` string clears the key
/// (→ default black). Only mechanical fields appear here — never cutouts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PanelEdit {
    /// Panel width in HP (`hp`).
    #[serde(default)]
    pub hp: Option<u16>,
    /// Material finish (`finish`) — a name or `#rrggbb`; empty string removes it.
    #[serde(default)]
    pub finish: Option<String>,
    /// Panel thickness in mm (`thickness_mm`).
    #[serde(default)]
    pub thickness_mm: Option<f64>,
}

impl PanelEdit {
    /// Whether this edit would change anything.
    pub fn is_empty(&self) -> bool {
        self.hp.is_none() && self.finish.is_none() && self.thickness_mm.is_none()
    }
}

/// Apply `edit` to a panel spec's TOML text, preserving formatting, and return
/// the new text. Does not write to disk — see [`edit_panel`].
pub fn apply_panel_edit_str(toml: &str, edit: &PanelEdit) -> Result<String, PanelEditError> {
    if let Some(hp) = edit.hp {
        if !HP_RANGE.contains(&hp) {
            return Err(PanelEditError::BadHp);
        }
    }
    if let Some(t) = edit.thickness_mm {
        if !THICKNESS_RANGE.contains(&t) {
            return Err(PanelEditError::BadThickness);
        }
    }
    if let Some(f) = &edit.finish {
        if !f.trim().is_empty() && !PanelFinish::is_recognized(f) {
            return Err(PanelEditError::BadFinish(f.clone()));
        }
    }

    let mut doc: DocumentMut = toml.parse()?;
    if let Some(hp) = edit.hp {
        doc["hp"] = value(i64::from(hp));
    }
    if let Some(t) = edit.thickness_mm {
        doc["thickness_mm"] = value(t);
    }
    if let Some(f) = &edit.finish {
        if f.trim().is_empty() {
            doc.as_table_mut().remove("finish");
        } else {
            doc["finish"] = value(f.trim());
        }
    }

    let out = doc.to_string();
    // Fail loudly on a corrupt result rather than persisting it.
    PanelFile::from_toml(&out).map_err(|e| PanelEditError::Invalid(e.to_string()))?;
    Ok(out)
}

/// Apply `edit` to the panel spec at `path` in place, preserving formatting.
pub fn edit_panel(path: &Path, edit: &PanelEdit) -> Result<(), PanelEditError> {
    let text = std::fs::read_to_string(path).map_err(|source| PanelEditError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let out = apply_panel_edit_str(&text, edit)?;
    std::fs::write(path, out).map_err(|source| PanelEditError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# Panel for the slew limiter.
format = "eurorack"
hp = 8
thickness_mm = 1.6

[[cutouts]]        # RATE pot — topology, must never be touched
x_mm = 20.32
y_mm = 100.0
footprint = "Alpha9mm"
refdes = "RV1"
label = "RATE"
"#;

    #[test]
    fn edits_hp_and_finish_preserving_cutouts_and_comments() {
        let edit = PanelEdit {
            hp: Some(4),
            finish: Some("silver".into()),
            thickness_mm: None,
        };
        let out = apply_panel_edit_str(SAMPLE, &edit).expect("apply");
        // Cutout topology + comments survive.
        assert!(out.contains("# RATE pot — topology, must never be touched"));
        assert!(out.contains(r#"footprint = "Alpha9mm""#));
        assert!(out.contains(r#"refdes = "RV1""#));
        // New values landed and re-parse.
        let pf = PanelFile::from_toml(&out).unwrap();
        assert_eq!(pf.hp, Some(4));
        assert_eq!(pf.finish.as_deref(), Some("silver"));
        assert_eq!(pf.thickness_mm, 1.6);
    }

    #[test]
    fn empty_finish_removes_the_key() {
        let with_finish = apply_panel_edit_str(
            SAMPLE,
            &PanelEdit {
                finish: Some("red".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(with_finish.contains("finish = \"red\""));
        let cleared = apply_panel_edit_str(
            &with_finish,
            &PanelEdit {
                finish: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!cleared.contains("finish ="));
        assert_eq!(PanelFile::from_toml(&cleared).unwrap().finish, None);
    }

    #[test]
    fn accepts_hex_finish() {
        let out = apply_panel_edit_str(
            SAMPLE,
            &PanelEdit {
                finish: Some("#1a2b3c".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            PanelFile::from_toml(&out).unwrap().finish.as_deref(),
            Some("#1a2b3c")
        );
    }

    #[test]
    fn rejects_out_of_range_hp() {
        assert!(matches!(
            apply_panel_edit_str(
                SAMPLE,
                &PanelEdit {
                    hp: Some(0),
                    ..Default::default()
                }
            ),
            Err(PanelEditError::BadHp)
        ));
        assert!(matches!(
            apply_panel_edit_str(
                SAMPLE,
                &PanelEdit {
                    hp: Some(200),
                    ..Default::default()
                }
            ),
            Err(PanelEditError::BadHp)
        ));
    }

    #[test]
    fn rejects_unknown_finish() {
        assert!(matches!(
            apply_panel_edit_str(
                SAMPLE,
                &PanelEdit {
                    finish: Some("chartreuse".into()),
                    ..Default::default()
                }
            ),
            Err(PanelEditError::BadFinish(_))
        ));
    }
}
