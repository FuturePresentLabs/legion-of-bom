//! Read and write a circuit's hand placement from the dashboard (cll.1).
//!
//! `GET  /api/circuits/{name}/placement` — the current `<name>.placement.toml`,
//! both as authored (patterns and overrides) and as expanded (refdes → panel
//! point). The editor needs both: it draws the expanded points, but it must show
//! and preserve the *intent* — "these three jacks are a column on 13.5mm pitch".
//!
//! `POST /api/circuits/{name}/placement` — apply a batch of
//! [`PlacementOp`]s and STAGE the result (`git add`), never commit: lob writes +
//! stages, the human batches and commits (DESIGN 2.5), exactly as `/api/edit`
//! does for `lob.toml`.
//!
//! Two guards sit in front of the writer:
//!
//! * **The circuit must be built.** The write guard in
//!   [`legion_of_bom_core::placement_edit`] refuses a refdes the circuit does not
//!   have, and the cached netlist (`out/<name>/<name>.net`) is where that list
//!   comes from. No netlist, no guard — so a placement written then would be
//!   unchecked, and this returns 409 instead.
//! * **The circuit must have a source.** An imported circuit is somebody else's
//!   finished board; nothing downstream would ever read a placement file for it.
//!
//! The coherence chain this endpoint feeds is deliberate and must not be
//! short-circuited: `placement.toml` overrides the panel's anchors, the board
//! follows the placement, and the panel is derived back from the board. This
//! writes the *first* link only — never a cutout position.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use legion_of_bom_core::placement::{board_view, PlacedControl};
use legion_of_bom_core::{
    edit_placement, git_stage, ops_for_targets, ops_to_reach, parse_netlist_file, staged_paths,
    BuiltinCutouts, CircuitSource, ControlKind, CutoutShape, Moved, PlacementEditError,
    PlacementFile, PlacementOp, Point, ProjectView,
};

use crate::state::AppState;

/// The placement as it stands: what the file says, and what that expands to.
#[derive(Debug, Serialize)]
pub struct PlacementDoc {
    /// Path to the placement file, relative to the repo root. Reported even when
    /// the file does not exist yet — that is where a first edit will write it.
    pub path: String,
    /// Whether the file is on disk. `false` means an untouched circuit, not an
    /// error: every field below is then empty.
    pub exists: bool,
    /// The authored file: `hp`, `[controls]` overrides, `[[patterns.*]]`.
    pub file: PlacementFile,
    /// Panel-space positions the file expands to, refdes → point.
    pub positions: HashMap<String, Point>,
}

/// One edit request, in exactly one of three forms.
///
/// The three exist because the dashboard's gestures are three different shapes,
/// and turning any of them into file operations is placement reasoning that
/// belongs in the core library rather than in a browser (DESIGN 2.2):
///
/// * `ops` — already file-shaped. "Make these a column on 13.5mm pitch."
/// * `targets` — "these parts end up here": a drag, a nudge, align, mirror.
///   [`ops_for_targets`] decides which patterns survive it.
/// * `restore` — "make the file say this again", which is how undo works for
///   every tool at once. Still applied as operations, so formatting survives.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementRequest {
    #[serde(default)]
    pub ops: Vec<PlacementOp>,
    /// refdes → panel-space point.
    #[serde(default)]
    pub targets: Option<BTreeMap<String, Point>>,
    #[serde(default)]
    pub restore: Option<PlacementFile>,
}

/// What the write did — the same `written`/`staged` shape `/api/edit` returns,
/// plus what the new file means.
#[derive(Debug, Serialize)]
pub struct PlacementWriteResult {
    pub written: bool,
    pub staged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_error: Option<String>,
    pub staged_files: Vec<String>,
    /// Path written, relative to the repo root.
    pub path: String,
    /// The file as authored after the edit. The editor keeps this so it can
    /// compute an exact inverse of what it just did — undo, without the client
    /// reimplementing pattern expansion.
    pub file: PlacementFile,
    /// Panel-space positions after the edit.
    pub positions: HashMap<String, Point>,
    /// Parts that moved without being named in the edit, because a pattern they
    /// belong to was reshaped. The UI is expected to say so out loud.
    pub side_effects: Vec<Moved>,
}

/// The board as the editor draws it: panel geometry, every part in panel space,
/// and which of them are panel hardware a person may move.
#[derive(Debug, Serialize)]
pub struct ControlsDto {
    /// `false` when there is no built board yet — the editor says so rather than
    /// showing an empty panel that looks like a placement decision.
    pub built: bool,
    pub width_mm: f64,
    pub height_mm: f64,
    pub hp: u16,
    pub parts: Vec<PartDto>,
}

/// One part in panel space. `panel: false` parts are board internals the placer
/// owns; the editor draws them dimmed for context and refuses to move them.
#[derive(Debug, Serialize)]
pub struct PartDto {
    pub refdes: String,
    pub value: String,
    pub footprint: String,
    /// Mount point in panel space — where the hardware comes through the panel.
    pub x: f64,
    pub y: f64,
    pub rotation_deg: f64,
    pub back: bool,
    pub panel: bool,
    /// `pot` / `switch` / `led` / `jack`, for panel hardware only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// The panel hole: a diameter, or a rounded rectangle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hole: Option<HoleDto>,
    /// The mechanical envelope `[w, h]` the hardware needs — a knob's skirt, a
    /// jack's nut. Bigger than the hole, and the figure clearance is measured in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope_mm: Option<[f64; 2]>,
}

/// A panel hole, flattened for the SVG that draws it.
#[derive(Debug, Serialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum HoleDto {
    Circle {
        diameter_mm: f64,
    },
    Rect {
        width_mm: f64,
        height_mm: f64,
        corner_radius_mm: f64,
    },
}

/// `GET /api/circuits/{name}/placement`
pub async fn get(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let root = state.root().to_path_buf();
    let rel = match resolve(&state, &name) {
        Ok(rel) => rel,
        Err((status, msg)) => return error(status, &msg),
    };
    match tokio::task::spawn_blocking(move || read_doc(&root, &rel)).await {
        Ok(Ok(doc)) => Json(doc).into_response(),
        Ok(Err(e)) => error(StatusCode::UNPROCESSABLE_ENTITY, &e),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "placement task panicked"),
    }
}

/// `POST /api/circuits/{name}/placement`
pub async fn post(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<PlacementRequest>,
) -> Response {
    let forms = usize::from(!req.ops.is_empty())
        + usize::from(req.targets.is_some())
        + usize::from(req.restore.is_some());
    if forms == 0 {
        return error(
            StatusCode::BAD_REQUEST,
            "empty edit — send one of `ops`, `targets` or `restore`",
        );
    }
    if forms > 1 {
        return error(
            StatusCode::BAD_REQUEST,
            "send exactly one of `ops`, `targets` or `restore` — they would fight",
        );
    }
    let root = state.root().to_path_buf();
    let rel = match resolve(&state, &name) {
        Ok(rel) => rel,
        Err((status, msg)) => return error(status, &msg),
    };
    match tokio::task::spawn_blocking(move || write_placement(&root, &name, &rel, &req)).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => error(status_for(&e), &e.to_string()),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "placement task panicked"),
    }
}

/// `GET /api/circuits/{name}/controls` — the board in panel space.
pub async fn controls(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    // The circuit must exist, but a placement path is not needed to *look*.
    match state.project() {
        Ok(v) if v.circuit(&name).is_none() => {
            return error(StatusCode::NOT_FOUND, &format!("no circuit '{name}'"))
        }
        Ok(_) => {}
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
    let root = state.root().to_path_buf();
    match tokio::task::spawn_blocking(move || read_controls(&root, &name)).await {
        Ok(Ok(dto)) => Json(dto).into_response(),
        Ok(Err(e)) => error(StatusCode::UNPROCESSABLE_ENTITY, &e),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "controls task panicked"),
    }
}

/// Why a write did not happen. Kept separate from
/// [`PlacementEditError`] so "the circuit has not been built" — a precondition of
/// the *endpoint*, not of the writer — gets its own status and its own message.
#[derive(Debug, thiserror::Error)]
enum WriteError {
    #[error("'{0}' has not been built — build it first so the editor knows which parts exist")]
    NotBuilt(String),
    #[error(transparent)]
    Edit(#[from] PlacementEditError),
}

// ---------------------------------------------------------------------------
// Blocking helpers (run under spawn_blocking)
// ---------------------------------------------------------------------------

fn read_doc(root: &FsPath, rel: &FsPath) -> Result<PlacementDoc, String> {
    let path = root.join(rel);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PlacementDoc {
                path: rel.display().to_string(),
                exists: false,
                file: PlacementFile::default(),
                positions: HashMap::new(),
            })
        }
        Err(e) => return Err(format!("reading {}: {e}", rel.display())),
    };
    let file = PlacementFile::from_toml(&text).map_err(|e| e.to_string())?;
    let positions = file.positions().map_err(|e| e.to_string())?;
    Ok(PlacementDoc {
        path: rel.display().to_string(),
        exists: true,
        file,
        positions,
    })
}

/// The built board read into panel space. Both inputs are the *cached* build
/// products, so this needs no KiCad and no SKiDL run — the editor opens instantly
/// and works offline, like the BOM endpoint.
fn read_controls(root: &FsPath, name: &str) -> Result<ControlsDto, String> {
    let dir = root.join("out").join(name);
    let (board, netlist) = (
        dir.join(format!("{name}.kicad_pcb")),
        dir.join(format!("{name}.net")),
    );
    let not_built = || ControlsDto {
        built: false,
        width_mm: 0.0,
        height_mm: 0.0,
        hp: 0,
        parts: Vec::new(),
    };
    let (Ok(pcb), Ok(circuit)) = (
        std::fs::read_to_string(&board),
        parse_netlist_file(&netlist),
    ) else {
        return Ok(not_built());
    };
    let view = board_view(&pcb, &circuit, &BuiltinCutouts)?;
    Ok(ControlsDto {
        built: true,
        width_mm: view.width_mm,
        height_mm: view.height_mm,
        hp: view.hp,
        parts: view.parts.iter().map(part_dto).collect(),
    })
}

fn part_dto(p: &PlacedControl) -> PartDto {
    let hole = p.cutout.as_ref().map(|c| match c.shape {
        CutoutShape::Circle { diameter_mm } => HoleDto::Circle { diameter_mm },
        CutoutShape::RoundedRect {
            width_mm,
            height_mm,
            corner_radius_mm,
        } => HoleDto::Rect {
            width_mm,
            height_mm,
            corner_radius_mm,
        },
    });
    PartDto {
        refdes: p.refdes.clone(),
        value: p.value.clone(),
        footprint: p.footprint.clone(),
        x: p.point.x,
        y: p.point.y,
        rotation_deg: p.rotation_deg,
        back: p.back,
        panel: p.cutout.is_some(),
        kind: p.cutout.as_ref().map(|c| match c.kind {
            ControlKind::Pot => "pot",
            ControlKind::Switch => "switch",
            ControlKind::Led => "led",
            ControlKind::Jack => "jack",
        }),
        hole,
        envelope_mm: p
            .cutout
            .as_ref()
            .map(|c| [c.envelope_mm.0, c.envelope_mm.1]),
    }
}

fn write_placement(
    root: &FsPath,
    name: &str,
    rel: &FsPath,
    req: &PlacementRequest,
) -> Result<PlacementWriteResult, WriteError> {
    let known = circuit_refdes(root, name)?;
    let path = root.join(rel);
    let ops = resolve_ops(&path, req)?;

    // `targets` and `restore` are requests about a *result*, so asking for one
    // the file already produces is success with nothing to do — not the empty-
    // edit error a caller sending `ops: []` deserves.
    if ops.is_empty() {
        let doc = read_doc(root, rel).map_err(PlacementEditError::Invalid)?;
        return Ok(PlacementWriteResult {
            written: false,
            staged: false,
            git_error: None,
            staged_files: Vec::new(),
            path: rel.display().to_string(),
            file: doc.file,
            positions: doc.positions,
            side_effects: Vec::new(),
        });
    }

    // A failed write is a hard error; nothing that matters changed on disk.
    let edit = edit_placement(&path, &ops, &known)?;

    // The file is now written. Staging is best-effort — report it, don't fail.
    let mut result = PlacementWriteResult {
        written: true,
        staged: false,
        git_error: None,
        staged_files: Vec::new(),
        path: rel.display().to_string(),
        file: edit.file,
        positions: edit.positions,
        side_effects: edit.side_effects,
    };
    match git_stage(root, &[rel]) {
        Ok(()) => {
            result.staged = true;
            result.staged_files = staged_paths(root);
        }
        Err(e) => result.git_error = Some(e.to_string()),
    }
    Ok(result)
}

/// Reduce a request to the operations it means. `targets` and `restore` are
/// resolved against the file as it stands, so what a drag writes depends on what
/// the file currently says — which is the whole point of editing intent.
fn resolve_ops(path: &FsPath, req: &PlacementRequest) -> Result<Vec<PlacementOp>, WriteError> {
    if !req.ops.is_empty() {
        return Ok(req.ops.clone());
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(PlacementEditError::Io {
                path: path.to_path_buf(),
                source,
            }
            .into())
        }
    };
    // An unreadable file must not be silently replaced by a derived edit.
    let current =
        PlacementFile::from_toml(&text).map_err(|e| PlacementEditError::Stale(e.to_string()))?;
    Ok(match (&req.targets, &req.restore) {
        (Some(t), _) => ops_for_targets(&current, t),
        (_, Some(want)) => ops_to_reach(&current, want),
        _ => Vec::new(),
    })
}

/// Every reference designator the built circuit actually has, from the cached
/// netlist. This is what makes the writer's guard real rather than advisory.
fn circuit_refdes(root: &FsPath, name: &str) -> Result<HashSet<String>, WriteError> {
    let netlist = root.join("out").join(name).join(format!("{name}.net"));
    let model = parse_netlist_file(&netlist).map_err(|_| WriteError::NotBuilt(name.to_string()))?;
    Ok(model.parts().iter().map(|p| p.refdes.0.clone()).collect())
}

// ---------------------------------------------------------------------------
// Resolution + error mapping
// ---------------------------------------------------------------------------

/// The placement file for a circuit, relative to the repo root:
/// `<dir of the circuit source>/<circuit name>.placement.toml` — the exact path
/// the CLI reads, so what the dashboard writes is what the next build consumes.
fn resolve(state: &AppState, name: &str) -> Result<PathBuf, (StatusCode, String)> {
    let view = state
        .project()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    placement_rel(&view, name)
}

fn placement_rel(view: &ProjectView, name: &str) -> Result<PathBuf, (StatusCode, String)> {
    let circuit = view
        .circuit(name)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no circuit '{name}'")))?;
    // An imported circuit is a finished board with no definition to place into —
    // nothing downstream would read a placement file for it.
    let source = circuit.source.as_deref().ok_or_else(|| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "imported circuit — it has no source to lay out".to_string(),
        )
    })?;
    let dir = FsPath::new(source).parent().unwrap_or(FsPath::new(""));
    Ok(dir.join(format!("{name}.placement.toml")))
}

fn status_for(e: &WriteError) -> StatusCode {
    let edit = match e {
        // A precondition of the endpoint: the circuit exists and the request is
        // well-formed, but there is nothing yet to check the refdes against.
        WriteError::NotBuilt(_) => return StatusCode::CONFLICT,
        WriteError::Edit(e) => e,
    };
    match edit {
        // The guard and the degenerate-pattern checks are all "your request does
        // not describe a placement", which is the request's problem.
        PlacementEditError::UnknownRefdes(_)
        | PlacementEditError::EmptyPattern
        | PlacementEditError::RepeatedRefdes(_)
        | PlacementEditError::EmptyGrid
        | PlacementEditError::NotFinite(_)
        | PlacementEditError::ZeroPitch => StatusCode::BAD_REQUEST,
        // The file on disk is the problem, not the request — it must be fixed by
        // hand before the editor can touch it.
        PlacementEditError::Parse(_)
        | PlacementEditError::Stale(_)
        | PlacementEditError::NotATable(_) => StatusCode::CONFLICT,
        // The edit was applied and the result did not survive validation. Nothing
        // was written; this is the writer refusing to corrupt the file.
        PlacementEditError::Invalid(_) | PlacementEditError::Conflict(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        PlacementEditError::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}
