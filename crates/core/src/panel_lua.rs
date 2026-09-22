//! Panel layout *conventions* as Lua scripts, loaded from disk at runtime —
//! not `include_str!`'d, so editing a `.lua` file and re-running `lob panel
//! derive`/`lob board` picks up the change with zero Rust recompilation.
//!
//! Same bounded-host-API shape `speedy`'s `src/lua_game.rs` already uses
//! (sandboxed stdlib, a narrow required-function contract validated at load
//! time, serde-table marshaling at the boundary): the split is deliberate.
//! *Verified hardware data* (a jack's real mounting-hole diameter, a pot's
//! real envelope) stays Rust — a single source of truth, wrong data there is
//! a correctness bug, not a style choice. *Arrangement convention* (which
//! controls, at what position, in what order — "fuzz pedal: jacks on the
//! sides, power on top") is the part that's genuinely one-off per pedal
//! style and would otherwise mean a new hardcoded Rust function (and a
//! recompile) for every layout idea — that part is what moves into Lua.
//!
//! Contract a script must satisfy: define a global function
//! `layout(spec) -> {cutout, cutout, ...}`, where `spec` carries the
//! enclosure dimensions, pot refdes, and the real hardware catalog (so the
//! script positions things but never invents a hole size), and each
//! returned cutout is a table with `x_mm`, `y_mm`, `footprint`, and optional
//! `rotation_deg` (default 0), `refdes`, `label`, `role`
//! (`"io"`/`"cv"`/`"knob"`/`"switch"`/`"attenuverter"`, unset = plain).
//! `centered_row(available_w, count, envelope_w)` is exposed as a host
//! helper — the row-centering math every layout needs, so a script doesn't
//! reimplement it slightly differently each time (same function
//! `PedalPanel::fuzz_pedal` already used, just callable from Lua now too).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::panel::{Cutout, CutoutRole};

/// A control kind's real, verified hole/envelope size — the data half of
/// the boundary. Mirrors [`crate::pedal_panel::PedalCutouts`]'s table; a
/// script reads these, never invents its own.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct HardwareSpec {
    pub diameter_mm: f64,
    pub envelope_mm: (f64, f64),
}

/// The full hardware catalog passed to a layout script — one entry per
/// control kind it might place.
#[derive(Debug, Clone, Serialize)]
pub struct HardwareCatalog {
    pub jack: HardwareSpec,
    pub dc_jack: HardwareSpec,
    pub pot: HardwareSpec,
    pub footswitch: HardwareSpec,
    pub led: HardwareSpec,
}

/// The observation a layout script receives: real enclosure geometry, real
/// hardware sizes, and whatever board parts should be anchored where — never
/// free text, same "bounded, typed" boundary this crate's `ooda` decisions
/// already keep.
#[derive(Debug, Clone, Serialize)]
pub struct LayoutSpec {
    pub width_mm: f64,
    pub height_mm: f64,
    pub edge_mm: f64,
    pub gap_mm: f64,
    pub pot_refdes: (String, String),
    pub hardware: HardwareCatalog,
}

/// One cutout as a script returns it — the wire shape `layout()` must
/// produce, before conversion to the real [`Cutout`] type (which needs a
/// parsed [`CutoutRole`], not a bare string).
#[derive(Debug, Clone, Deserialize)]
struct LuaCutout {
    x_mm: f64,
    y_mm: f64,
    #[serde(default)]
    rotation_deg: f64,
    footprint: String,
    #[serde(default)]
    refdes: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

/// A `.lua` layout script's `role` string didn't match a known
/// [`CutoutRole`] — fails loud rather than silently dropping the styling
/// (tenet: fail loud, no silent fallback).
#[derive(Debug, thiserror::Error)]
pub enum PanelScriptError {
    #[error("panel script {path}: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: mlua::Error,
    },
    #[error("panel script {path} must define a global function `layout(spec)`")]
    MissingLayoutFn { path: PathBuf },
    #[error("panel script {path}: layout() call failed: {source}")]
    Call {
        path: PathBuf,
        #[source]
        source: mlua::Error,
    },
    #[error("panel script {path}: cutout {index} has unknown role {role:?} (expected io/cv/knob/switch/attenuverter)")]
    UnknownRole {
        path: PathBuf,
        index: usize,
        role: String,
    },
}

fn parse_role(role: &str) -> Option<CutoutRole> {
    match role {
        "io" => Some(CutoutRole::Io),
        "cv" => Some(CutoutRole::Cv),
        "knob" => Some(CutoutRole::Knob),
        "switch" => Some(CutoutRole::Switch),
        "attenuverter" => Some(CutoutRole::Attenuverter),
        _ => None,
    }
}

/// A loaded, ready-to-call panel layout script.
pub struct PanelScript {
    lua: mlua::Lua,
    path: PathBuf,
}

impl PanelScript {
    /// Loads a layout script from disk. Sandboxed to `table`/`string`/`math`
    /// (no filesystem, process, package, or network libraries reachable
    /// from the script), same allowlist `speedy::LuaGame` uses — a panel
    /// layout has no legitimate reason to touch any of those.
    pub fn load(path: &Path) -> Result<Self, PanelScriptError> {
        let lua = mlua::Lua::new_with(
            mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH,
            mlua::LuaOptions::default(),
        )
        .map_err(|source| PanelScriptError::Load {
            path: path.to_path_buf(),
            source,
        })?;

        lua.globals()
            .set(
                "centered_row",
                lua.create_function(|_, (available_w, count, envelope_w): (f64, i64, f64)| {
                    Ok(crate::pedal_panel::centered_row(
                        available_w,
                        count.max(0) as usize,
                        envelope_w,
                    ))
                })
                .map_err(|source| PanelScriptError::Load {
                    path: path.to_path_buf(),
                    source,
                })?,
            )
            .map_err(|source| PanelScriptError::Load {
                path: path.to_path_buf(),
                source,
            })?;

        let src = std::fs::read_to_string(path).map_err(|e| PanelScriptError::Load {
            path: path.to_path_buf(),
            source: mlua::Error::RuntimeError(format!("read {}: {e}", path.display())),
        })?;
        lua.load(src)
            .set_name(path.to_string_lossy())
            .exec()
            .map_err(|source| PanelScriptError::Load {
                path: path.to_path_buf(),
                source,
            })?;

        if !lua
            .globals()
            .get::<mlua::Value>("layout")
            .is_ok_and(|v| v.is_function())
        {
            return Err(PanelScriptError::MissingLayoutFn {
                path: path.to_path_buf(),
            });
        }

        Ok(PanelScript {
            lua,
            path: path.to_path_buf(),
        })
    }

    /// Runs `layout(spec)` and converts the result to real [`Cutout`]s.
    pub fn layout(&self, spec: &LayoutSpec) -> Result<Vec<Cutout>, PanelScriptError> {
        use mlua::LuaSerdeExt;

        let layout_fn: mlua::Function =
            self.lua
                .globals()
                .get("layout")
                .map_err(|source| PanelScriptError::Call {
                    path: self.path.clone(),
                    source,
                })?;
        let spec_value = self
            .lua
            .to_value(spec)
            .map_err(|source| PanelScriptError::Call {
                path: self.path.clone(),
                source,
            })?;
        let result: mlua::Value =
            layout_fn
                .call(spec_value)
                .map_err(|source| PanelScriptError::Call {
                    path: self.path.clone(),
                    source,
                })?;
        let lua_cutouts: Vec<LuaCutout> =
            self.lua
                .from_value(result)
                .map_err(|source| PanelScriptError::Call {
                    path: self.path.clone(),
                    source,
                })?;

        lua_cutouts
            .into_iter()
            .enumerate()
            .map(|(index, c)| {
                let role = match c.role {
                    Some(r) => {
                        Some(parse_role(&r).ok_or_else(|| PanelScriptError::UnknownRole {
                            path: self.path.clone(),
                            index,
                            role: r,
                        })?)
                    }
                    None => None,
                };
                Ok(Cutout {
                    x_mm: c.x_mm,
                    y_mm: c.y_mm,
                    rotation_deg: c.rotation_deg,
                    footprint: c.footprint,
                    refdes: c.refdes,
                    label: c.label,
                    role,
                })
            })
            .collect()
    }
}

/// Where panel layout scripts live, highest precedence first: an explicit
/// `LOB_PANEL_SCRIPTS_DIR` override, then walking up from the working
/// directory looking for this checkout's own `assets/panels` (the dev loop
/// the "no recompile" goal is actually for — edit the script, rerun `lob`,
/// no `cargo build`), then the same global `~/.local/share/legion-of-bom`
/// tree [`crate::parts::default_parts_dir`] already uses for user data.
pub fn panel_script_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LOB_PANEL_SCRIPTS_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Some(dir) = find_upward("assets/panels") {
        return Some(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    let global = base.join("legion-of-bom").join("panels");
    global.is_dir().then_some(global)
}

/// Walks up from the working directory looking for `relative` — e.g. this
/// checkout's own `assets/panels` when run from anywhere inside it.
fn find_upward(relative: &str) -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(relative);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_catalog() -> HardwareCatalog {
        HardwareCatalog {
            jack: HardwareSpec {
                diameter_mm: 9.5,
                envelope_mm: (20.0, 20.0),
            },
            dc_jack: HardwareSpec {
                diameter_mm: 8.0,
                envelope_mm: (16.0, 16.0),
            },
            pot: HardwareSpec {
                diameter_mm: 7.0,
                envelope_mm: (18.0, 18.0),
            },
            footswitch: HardwareSpec {
                diameter_mm: 12.0,
                envelope_mm: (20.0, 20.0),
            },
            led: HardwareSpec {
                diameter_mm: 5.0,
                envelope_mm: (6.0, 6.0),
            },
        }
    }

    #[test]
    fn a_minimal_script_round_trips_one_cutout() {
        let dir = tempfile_dir();
        let path = dir.join("minimal.lua");
        std::fs::write(
            &path,
            r#"
            function layout(spec)
                return {
                    { x_mm = spec.width_mm / 2, y_mm = 10, footprint = "LED_5mm", role = "knob" },
                }
            end
            "#,
        )
        .unwrap();
        let script = PanelScript::load(&path).unwrap();
        let spec = LayoutSpec {
            width_mm: 60.0,
            height_mm: 112.0,
            edge_mm: 1.0,
            gap_mm: 3.0,
            pot_refdes: ("RV1".into(), "RV2".into()),
            hardware: test_catalog(),
        };
        let cutouts = script.layout(&spec).unwrap();
        assert_eq!(cutouts.len(), 1);
        assert_eq!(cutouts[0].x_mm, 30.0);
        assert_eq!(cutouts[0].role, Some(CutoutRole::Knob));
    }

    #[test]
    fn sandbox_blocks_filesystem_access() {
        let dir = tempfile_dir();
        let path = dir.join("escape.lua");
        std::fs::write(
            &path,
            r#"
            function layout(spec)
                io.open("/etc/passwd", "r")
                return {}
            end
            "#,
        )
        .unwrap();
        let script = PanelScript::load(&path).unwrap();
        let spec = LayoutSpec {
            width_mm: 60.0,
            height_mm: 112.0,
            edge_mm: 1.0,
            gap_mm: 3.0,
            pot_refdes: ("RV1".into(), "RV2".into()),
            hardware: test_catalog(),
        };
        assert!(
            script.layout(&spec).is_err(),
            "io library must not be reachable"
        );
    }

    #[test]
    fn unknown_role_fails_loud_not_silently() {
        let dir = tempfile_dir();
        let path = dir.join("bad_role.lua");
        std::fs::write(
            &path,
            r#"
            function layout(spec)
                return { { x_mm = 0, y_mm = 0, footprint = "x", role = "not_a_real_role" } }
            end
            "#,
        )
        .unwrap();
        let script = PanelScript::load(&path).unwrap();
        let spec = LayoutSpec {
            width_mm: 60.0,
            height_mm: 112.0,
            edge_mm: 1.0,
            gap_mm: 3.0,
            pot_refdes: ("RV1".into(), "RV2".into()),
            hardware: test_catalog(),
        };
        assert!(matches!(
            script.layout(&spec),
            Err(PanelScriptError::UnknownRole { .. })
        ));
    }

    #[test]
    fn centered_row_helper_is_reachable_from_lua() {
        let dir = tempfile_dir();
        let path = dir.join("uses_helper.lua");
        std::fs::write(
            &path,
            r#"
            function layout(spec)
                local xs = centered_row(spec.width_mm, 2, spec.hardware.pot.envelope_mm[1])
                local out = {}
                for i, x in ipairs(xs) do
                    out[i] = { x_mm = x, y_mm = 50, footprint = "Potentiometer_16mm", role = "knob" }
                end
                return out
            end
            "#,
        )
        .unwrap();
        let script = PanelScript::load(&path).unwrap();
        let spec = LayoutSpec {
            width_mm: 60.0,
            height_mm: 112.0,
            edge_mm: 1.0,
            gap_mm: 3.0,
            pot_refdes: ("RV1".into(), "RV2".into()),
            hardware: test_catalog(),
        };
        let cutouts = script.layout(&spec).unwrap();
        assert_eq!(cutouts.len(), 2);
        assert!(cutouts[0].x_mm < cutouts[1].x_mm);
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lob-panel-lua-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
