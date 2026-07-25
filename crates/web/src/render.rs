//! On-demand board + panel renders (beads bzg + jg4 + hk0).
//!
//! `GET /api/circuits/{name}/render?view=board-top|board-bottom|board-layout|panel`:
//! - **board-top/bottom** rasterize `out/<name>/<name>.kicad_pcb` (photoreal PNG,
//!   core `render_board_png`, kicad-cli), disk-cached by source mtime.
//! - **board-layout** is the flat 2D layout SVG (copper + silk + fab + edge, core
//!   `export_board_svg`, kicad-cli), cached alongside.
//! - **panel** is a flat 2D **SVG in the panel's real finish color** (core
//!   `panel_to_svg`, from the declared spec + the repo brand logo) — no kicad-cli.
//!
//! Board errors: missing kicad-cli → 503, unbuilt board → 404. Panel errors:
//! undeclared/absent spec → 404. Bad view → 400.

use std::hash::{Hash, Hasher};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use legion_of_bom_core::{
    default_image_cache_dir, export_board_svg, kicad_cli_path, panel_to_svg, parse_netlist_file,
    render_board_png, schematic_to_svg, strip_smd, Logo, PanelFile,
};

use crate::state::AppState;

#[derive(Deserialize)]
pub struct RenderQuery {
    #[serde(default)]
    view: Option<String>,
    /// `smd=0` hides surface-mount parts from a board view — the through-hole-only
    /// picture a builder of a mixed kit actually works on (a2r).
    #[serde(default)]
    smd: Option<u8>,
}

/// A rasterized board render or a vector panel.
enum Rendered {
    Png(Vec<u8>),
    Svg(String),
}

/// `GET /api/circuits/{name}/render?view=…` — a PNG (board) or SVG (panel).
pub async fn render(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<RenderQuery>,
) -> Response {
    let view = q.view.unwrap_or_else(|| "board-top".to_string());
    let show_smd = q.smd != Some(0);

    // The circuit must exist; grab its panel spec + the repo brand logo.
    let (panel_rel, logo_rel) = match state.project() {
        Ok(v) => match v.circuit(&name) {
            Some(c) => (c.panel.clone(), v.repo.logo.clone()),
            None => return err(StatusCode::NOT_FOUND, &format!("no circuit '{name}'")),
        },
        Err(e) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("repo unavailable: {e}"),
            )
        }
    };

    let root = state.root().to_path_buf();
    match tokio::task::spawn_blocking(move || {
        render_view(
            &root,
            &name,
            &view,
            show_smd,
            panel_rel.as_deref(),
            logo_rel.as_deref(),
        )
    })
    .await
    {
        Ok(Ok(Rendered::Png(b))) => png_response(b),
        Ok(Ok(Rendered::Svg(s))) => svg_response(s),
        Ok(Err(e)) => e.into_response(),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "render task panicked"),
    }
}

/// A render failure mapped to an HTTP status the dashboard can act on.
enum RenderErr {
    NoKicad,
    NotBuilt(String),
    NoPanel,
    BadView(String),
    Failed(String),
}

impl IntoResponse for RenderErr {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            RenderErr::NoKicad => (
                StatusCode::SERVICE_UNAVAILABLE,
                "kicad-cli not found — install KiCad to see board renders".to_string(),
            ),
            RenderErr::NotBuilt(m) => (StatusCode::NOT_FOUND, m),
            RenderErr::NoPanel => (
                StatusCode::NOT_FOUND,
                "no panel spec declared for this circuit".to_string(),
            ),
            RenderErr::BadView(v) => (
                StatusCode::BAD_REQUEST,
                format!("unknown view '{v}' (board-top | board-bottom | board-layout | panel | schematic)"),
            ),
            RenderErr::Failed(m) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("render failed: {m}"),
            ),
        };
        err(code, &msg)
    }
}

fn render_view(
    root: &FsPath,
    name: &str,
    view: &str,
    show_smd: bool,
    panel_rel: Option<&str>,
    logo_rel: Option<&str>,
) -> Result<Rendered, RenderErr> {
    match view {
        "panel" => {
            let rel = panel_rel.ok_or(RenderErr::NoPanel)?;
            let spec = root.join(rel);
            if !spec.is_file() {
                return Err(RenderErr::NoPanel);
            }
            // Cache keyed by the spec's mtime (its hp/finish/cutouts live there),
            // so an unchanged panel serves instantly instead of re-parsing the
            // logo and rebuilding the SVG on every poll.
            let cache = cache_path(&spec, view, "svg");
            if let Ok(svg) = std::fs::read_to_string(&cache) {
                return Ok(Rendered::Svg(svg));
            }
            let svg = render_panel_svg(root, &spec, name, logo_rel)?;
            write_cache(&cache, svg.as_bytes());
            Ok(Rendered::Svg(svg))
        }
        "board-top" | "board-bottom" => {
            let board = board_path(root, name)?;
            let back = view == "board-bottom";
            // Serve a cached render when the board hasn't changed since. The SMD
            // filter is part of the key — the two variants are different pictures.
            let key = if show_smd {
                view.to_string()
            } else {
                format!("{view}-tht")
            };
            let cache = cache_path(&board, &key, "png");
            if let Ok(bytes) = std::fs::read(&cache) {
                return Ok(Rendered::Png(bytes));
            }
            let kicad = kicad_cli_path().ok_or(RenderErr::NoKicad)?;
            let board = if show_smd {
                board
            } else {
                tht_only_board(&board, name)?
            };
            // bare=true (unpopulated, 3D models stripped) — the proven guide path.
            let png = render_board_png(&board, &kicad, true, back)
                .map_err(|e| RenderErr::Failed(e.to_string()))?
                .0;
            write_cache(&cache, &png);
            Ok(Rendered::Png(png))
        }
        "schematic" => {
            // Drawn from the parsed netlist — no kicad-cli, so it's always
            // available once the circuit has been built.
            let netlist = root.join("out").join(name).join(format!("{name}.net"));
            let model = parse_netlist_file(&netlist).map_err(|_| {
                RenderErr::NotBuilt(format!("circuit not built — run `lob build {name}`"))
            })?;
            let cache = cache_path(&netlist, view, "svg");
            if let Ok(svg) = std::fs::read_to_string(&cache) {
                return Ok(Rendered::Svg(svg));
            }
            let svg = schematic_to_svg(&model);
            write_cache(&cache, svg.as_bytes());
            Ok(Rendered::Svg(svg))
        }
        "board-layout" => {
            // The flat 2D layout: copper + silk + fab + edge, as a scalable SVG.
            let board = board_path(root, name)?;
            let key = if show_smd {
                view.to_string()
            } else {
                format!("{view}-tht")
            };
            let cache = cache_path(&board, &key, "svg");
            if let Ok(svg) = std::fs::read_to_string(&cache) {
                return Ok(Rendered::Svg(svg));
            }
            let kicad = kicad_cli_path().ok_or(RenderErr::NoKicad)?;
            let board = if show_smd {
                board
            } else {
                tht_only_board(&board, name)?
            };
            let svg =
                export_board_svg(&board, &kicad).map_err(|e| RenderErr::Failed(e.to_string()))?;
            write_cache(&cache, svg.as_bytes());
            Ok(Rendered::Svg(svg))
        }
        other => Err(RenderErr::BadView(other.to_string())),
    }
}

/// Build the panel SVG from its declared spec, in its finish color, with the
/// brand logo placed by the house rules (best-effort — a missing/bad logo is
/// simply omitted).
fn render_panel_svg(
    root: &FsPath,
    spec_path: &FsPath,
    name: &str,
    logo_rel: Option<&str>,
) -> Result<String, RenderErr> {
    let toml = std::fs::read_to_string(spec_path).map_err(|e| RenderErr::Failed(e.to_string()))?;
    let file = PanelFile::from_toml(&toml).map_err(|e| RenderErr::Failed(e.to_string()))?;
    let spec = file.to_spec().map_err(RenderErr::Failed)?;
    let finish = file.resolved_finish();
    let logo = logo_rel.and_then(|rel| {
        std::fs::read_to_string(root.join(rel))
            .ok()
            .and_then(|svg| Logo::from_svg(&svg).ok())
    });
    Ok(panel_to_svg(
        spec.as_ref(),
        &pretty_title(name),
        &finish,
        logo.as_ref(),
    ))
}

/// Write a through-hole-only copy of the board to the cache dir and return its
/// path, so `kicad-cli` renders the picture a builder of a mixed kit works on.
fn tht_only_board(board: &FsPath, name: &str) -> Result<PathBuf, RenderErr> {
    let src = std::fs::read_to_string(board).map_err(|e| RenderErr::Failed(e.to_string()))?;
    let out = default_image_cache_dir()
        .join("lob-render")
        .join(format!("{name}-tht.kicad_pcb"));
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, strip_smd(&src)).map_err(|e| RenderErr::Failed(e.to_string()))?;
    Ok(out)
}

/// The board file for `name`, or `NotBuilt` when it hasn't been built.
fn board_path(root: &FsPath, name: &str) -> Result<PathBuf, RenderErr> {
    let board = root
        .join("out")
        .join(name)
        .join(format!("{name}.kicad_pcb"));
    if board.is_file() {
        Ok(board)
    } else {
        Err(RenderErr::NotBuilt(format!(
            "board not built — run `lob build {name}`"
        )))
    }
}

/// Cache path keyed by the source file's absolute path + mtime + view, so a
/// changed board produces a fresh key (and the stale render is ignored).
fn cache_path(source: &FsPath, view: &str, ext: &str) -> PathBuf {
    let mtime = std::fs::metadata(source)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    view.hash(&mut h);
    mtime.hash(&mut h);
    default_image_cache_dir()
        .join("lob-render")
        .join(format!("{:016x}.{ext}", h.finish()))
}

/// Best-effort write to the render cache (creating the dir).
fn write_cache(path: &FsPath, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, bytes);
}

/// "slew_limiter" → "Slew Limiter" for the panel masthead.
fn pretty_title(name: &str) -> String {
    name.split(['_', '-'])
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn png_response(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

fn svg_response(svg: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        svg,
    )
        .into_response()
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, axum::Json(json!({ "error": msg }))).into_response()
}
