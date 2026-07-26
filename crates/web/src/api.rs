//! The dashboard API (p58.3 read surface + p58.5 metadata editing).
//!
//! Every endpoint is a view over the shared core read model — never a web-only
//! reimplementation (DESIGN 2.2). The repo/circuit endpoints serialize
//! [`legion_of_bom_core::ProjectView`] directly; BOM comes from the *cached*
//! netlist (`out/<name>/<name>.net`) so it needs no SKiDL run and works offline;
//! live pricing (Mouser) and panel-order status (Dolt) are best-effort overlays
//! that degrade to a flagged empty result when the key / tool is absent.
//!
//! The one mutating endpoint, [`edit`], writes only whitelisted *metadata*
//! (brand, build copy, kit) back to `lob.toml` and STAGES it — never commits, and
//! never touches circuit topology (DESIGN 1.3/2.5). Everything else is read-only.

use std::path::Path as FsPath;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use legion_of_bom_core::manifest::MANIFEST_NAME;
use legion_of_bom_core::{
    build_query, default_panel_orders_dir, edit_manifest, generate_bom, git_stage,
    parse_netlist_file, photo_source, read_crop, staged_paths, suggest_mpns, CircuitSource,
    EditError, ManifestEdit, MouserClient, PanelOrders, SourcingClients,
};

use crate::assets::content_type;
use crate::state::AppState;

/// `GET /api/repo` — the repo masthead: brand/meta + circuit count.
pub async fn repo(State(state): State<Arc<AppState>>) -> Response {
    match state.project() {
        Ok(view) => Json(RepoInfo {
            root: view.root.display().to_string(),
            name: view.repo.name,
            brand: view.repo.brand,
            logo: view.repo.logo,
            circuits: view.circuits.len(),
        })
        .into_response(),
        Err(e) => repo_error(e),
    }
}

/// `GET /api/circuits` — every circuit with its artifact inventory + freshness.
pub async fn circuits(State(state): State<Arc<AppState>>) -> Response {
    match state.project() {
        Ok(view) => Json(view.circuits).into_response(),
        Err(e) => repo_error(e),
    }
}

/// `GET /api/circuits/{name}` — one circuit's detail, or 404.
pub async fn circuit(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match state.project() {
        Ok(view) => match view.circuit(&name) {
            Some(c) => Json(c.clone()).into_response(),
            None => not_found(&format!("no circuit '{name}'")),
        },
        Err(e) => repo_error(e),
    }
}

/// `GET /api/circuits/{name}/source` — the circuit's SKiDL source, read-only.
/// Topology stays code (DESIGN 1.3); this is a viewer, not an editor.
pub async fn source(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let (src_rel, root) = match state.project() {
        Ok(v) => match v.circuit(&name) {
            Some(c) => (c.source.clone(), v.root.clone()),
            None => return not_found(&format!("no circuit '{name}'")),
        },
        Err(e) => return repo_error(e),
    };
    // An imported circuit is somebody else's finished board: there is no
    // definition to show, and saying so beats a misleading empty pane.
    let Some(src_rel) = src_rel else {
        return not_found("imported circuit — no source, it was defined elsewhere");
    };
    let path = root.join(&src_rel);
    match tokio::task::spawn_blocking(move || std::fs::read_to_string(&path)).await {
        Ok(Ok(content)) => Json(SourceDoc {
            name,
            path: src_rel,
            language: "python",
            content,
        })
        .into_response(),
        Ok(Err(_)) => not_found("source file not found on disk"),
        Err(_) => server_error("source task panicked"),
    }
}

/// `GET /api/circuits/{name}/bom?price=<bool>` — the BOM from the cached netlist.
/// `built:false` when the circuit hasn't been built yet; `priced:false` when live
/// pricing didn't run (no `MOUSER_API_KEY`, or `?price` omitted).
pub async fn bom(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<BomQuery>,
) -> Response {
    // Reject unknown circuits up front, using the shared model.
    match state.project() {
        Ok(view) if view.circuit(&name).is_none() => {
            return not_found(&format!("no circuit '{name}'"));
        }
        Ok(_) => {}
        Err(e) => return repo_error(e),
    }

    let root = state.root().to_path_buf();
    let (price, photos) = (q.price, q.photos);
    // fs parse (+ optional blocking Mouser/photo HTTP) off the async runtime.
    match tokio::task::spawn_blocking(move || load_bom(&root, &name, price, photos)).await {
        Ok(dto) => Json(dto).into_response(),
        Err(_) => server_error("BOM task panicked"),
    }
}

/// `GET /api/circuits/{name}/suggest?limit=N` — ranked real-MPN candidates for the
/// circuit's *generic* parts (those with no MPN), from the cached netlist. Powers
/// a future dashboard "pick an MPN" confirm action (lrr). SUGGEST-ONLY: this is a
/// read; confirming a candidate is a separate write the human triggers (never
/// silent-assign, per the okm gate). `built:false` when the circuit hasn't been
/// built; `sources` reports which distributors were consulted (Mouser needs a key;
/// LCSC is keyless). Best-effort — an absent key just drops that source.
pub async fn suggest(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<SuggestQuery>,
) -> Response {
    match state.project() {
        Ok(view) if view.circuit(&name).is_none() => {
            return not_found(&format!("no circuit '{name}'"));
        }
        Ok(_) => {}
        Err(e) => return repo_error(e),
    }
    let root = state.root().to_path_buf();
    let limit = q.limit.clamp(1, 10);
    // Network (Mouser/LCSC) + fs parse off the async runtime.
    match tokio::task::spawn_blocking(move || load_suggestions(&root, &name, limit)).await {
        Ok(dto) => Json(dto).into_response(),
        Err(_) => server_error("suggest task panicked"),
    }
}

/// `GET /api/circuits/{name}/orders` — panel-order status for this module.
/// `available:false` when the Dolt-backed order store can't be opened (e.g. no
/// `dolt` on PATH) — the dashboard shows "orders unavailable", not an error.
pub async fn orders(State(_state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match tokio::task::spawn_blocking(move || load_orders(&name)).await {
        Ok(dto) => Json(dto).into_response(),
        Err(_) => server_error("orders task panicked"),
    }
}

/// `POST /api/edit` — apply a whitelisted metadata edit to `lob.toml` and STAGE
/// it (`git add`). Never commits or pushes: lob writes + stages, the human
/// batches and commits (DESIGN 2.5). The write is format-preserving and can only
/// touch content fields — never circuit topology (DESIGN 1.3). The file is
/// written even if staging fails (e.g. not a git repo); the response says which.
pub async fn edit(State(state): State<Arc<AppState>>, Json(edit): Json<ManifestEdit>) -> Response {
    if edit.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "empty edit — no fields to change" })),
        )
            .into_response();
    }
    let root = state.root().to_path_buf();
    match tokio::task::spawn_blocking(move || apply_and_stage(&root, &edit)).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => (edit_status(&e), Json(json!({ "error": e.to_string() }))).into_response(),
        Err(_) => server_error("edit task panicked"),
    }
}

/// Write the edit (format-preserving, validated) then best-effort stage it.
fn apply_and_stage(root: &FsPath, edit: &ManifestEdit) -> Result<EditResult, EditError> {
    // A failed write is a hard error (nothing changed on disk that matters).
    edit_manifest(root, edit)?;
    // The file is now written. Staging is best-effort — report it, don't fail.
    let mut result = EditResult {
        written: true,
        staged: false,
        git_error: None,
        staged_files: Vec::new(),
    };
    match git_stage(root, &[FsPath::new(MANIFEST_NAME)]) {
        Ok(()) => {
            result.staged = true;
            result.staged_files = staged_paths(root);
        }
        Err(e) => result.git_error = Some(e.to_string()),
    }
    Ok(result)
}

fn edit_status(e: &EditError) -> StatusCode {
    match e {
        EditError::CircuitNotFound(_) => StatusCode::NOT_FOUND,
        EditError::Parse(_) | EditError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        EditError::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// `GET /artifacts/{name}/{*path}` — serve a built artifact file from
/// `out/<name>/…` (guide/vbom HTML, PDF, gerber zip, board file). Read-only, with
/// path-traversal guards.
pub async fn artifact(
    State(state): State<Arc<AppState>>,
    Path((name, rel)): Path<(String, String)>,
) -> Response {
    if is_unsafe(&name) || is_unsafe(&rel) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let base = state.root().join("out").join(&name);
    let full = base.join(&rel);
    // Redundant with is_unsafe, but cheap defence in depth.
    if !full.starts_with(&base) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let ct = content_type(&rel);
    match tokio::task::spawn_blocking(move || std::fs::read(&full)).await {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, ct),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

// ---------------------------------------------------------------------------
// Blocking helpers (run under spawn_blocking)
// ---------------------------------------------------------------------------

/// Build the BOM from the cached netlist; optionally overlay live Mouser pricing.
fn load_bom(root: &FsPath, name: &str, price: bool, photos: bool) -> BomDto {
    let netlist = root.join("out").join(name).join(format!("{name}.net"));
    let model = match parse_netlist_file(&netlist) {
        Ok(m) => m,
        // No cached netlist → the circuit hasn't been built yet.
        Err(_) => return BomDto::not_built(),
    };
    let mut bom = generate_bom(&model);

    let mut priced = false;
    if price {
        if let Ok(client) = MouserClient::from_env() {
            for line in bom.lines.iter_mut() {
                let Some(mpn) = line.mpn.clone() else {
                    continue;
                };
                if let Ok(Some(pp)) = client.search_mpn(&mpn) {
                    if let Some(unit) = pp.unit_price_at(line.qty() as u64) {
                        line.set_unit_price(unit);
                        priced = true;
                    }
                }
            }
        }
    }

    let total = priced
        .then(|| bom.lines.iter().filter_map(|l| l.ext_price).sum::<f64>())
        .filter(|t| *t > 0.0);
    let cache = legion_of_bom_core::default_image_cache_dir();
    let lines = bom
        .lines
        .iter()
        .map(|l| {
            let photo_src = photos.then(|| photo_source(l, &cache)).flatten();
            let crop = photo_src
                .as_deref()
                .and_then(|s| read_crop(&cache, s))
                .map(|c| [c.x, c.y, c.w, c.h]);
            BomLineDto {
                mpn: l.mpn.clone(),
                value: l.value.clone(),
                footprint: l.footprint.clone(),
                refdes: l.refdes.clone(),
                qty: l.qty(),
                unit_price: l.unit_price,
                ext_price: l.ext_price,
                image_url: l.image_url.clone(),
                photo_src,
                crop,
            }
        })
        .collect();
    BomDto {
        built: true,
        priced,
        total,
        lines,
    }
}

/// Build MPN suggestions from the cached netlist: for each part with no MPN,
/// grouped by (value, footprint), run the distributor searches and rank. Runs
/// under `spawn_blocking` (it does network I/O). Degrades to `built:false` when
/// there's no cached netlist, and to empty candidates when no source is available.
fn load_suggestions(root: &FsPath, name: &str, limit: usize) -> SuggestDto {
    let netlist = root.join("out").join(name).join(format!("{name}.net"));
    let model = match parse_netlist_file(&netlist) {
        Ok(m) => m,
        Err(_) => return SuggestDto::not_built(),
    };

    let clients = SourcingClients::from_env();
    let mut sources = Vec::new();
    if clients.mouser.is_some() {
        sources.push("mouser".to_string());
    }
    if clients.use_lcsc {
        sources.push("lcsc".to_string());
    }

    // One search per distinct generic part (value, footprint), collecting refdes.
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<(String, Option<String>), (legion_of_bom_core::Part, Vec<String>)> =
        BTreeMap::new();
    for part in model.parts() {
        if part.mpn.is_some() {
            continue;
        }
        groups
            .entry((part.value.clone(), part.footprint.clone()))
            .or_insert_with(|| (part.clone(), Vec::new()))
            .1
            .push(part.refdes.0.clone());
    }

    let parts = groups
        .into_iter()
        .map(|((value, footprint), (part, mut refdes))| {
            refdes.sort();
            let query = build_query(&part);
            let candidates = if clients.any() {
                suggest_mpns(&part, &clients, limit)
            } else {
                Vec::new()
            };
            SuggestPartDto {
                refdes,
                value,
                footprint,
                query,
                candidates: candidates.into_iter().map(CandidateDto::from).collect(),
            }
        })
        .collect();

    SuggestDto {
        built: true,
        sources,
        parts,
    }
}

/// Read panel-order rows for `module` from the Dolt-backed store. Any failure
/// (store missing, no `dolt`) degrades to `available:false`, empty list.
fn load_orders(module: &str) -> OrdersDto {
    let Ok(store) = PanelOrders::open(default_panel_orders_dir()) else {
        return OrdersDto::unavailable();
    };
    match store.list(module) {
        Ok(rows) => OrdersDto {
            available: true,
            orders: rows.into_iter().map(OrderDto::from).collect(),
        },
        Err(_) => OrdersDto::unavailable(),
    }
}

// ---------------------------------------------------------------------------
// Wire DTOs (core types don't derive Serialize; the web layer owns its shape)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SourceDoc {
    name: String,
    /// Repo-relative source path (e.g. `slew_limiter.py`).
    path: String,
    /// Highlighting hint — SKiDL is Python.
    language: &'static str,
    content: String,
}

#[derive(Serialize)]
struct RepoInfo {
    root: String,
    name: Option<String>,
    brand: Option<String>,
    logo: Option<String>,
    circuits: usize,
}

#[derive(Deserialize)]
pub struct BomQuery {
    #[serde(default)]
    price: bool,
    /// Resolve each line's photo (curated → Thonk → LCSC). Opt-in because an
    /// unseen photo costs a network round trip per line; once cached it is a
    /// file read. Needed by the crop editor, which cannot crop what it cannot
    /// name.
    #[serde(default)]
    photos: bool,
}

#[derive(Serialize)]
struct BomDto {
    /// Whether a cached netlist was found (i.e. the circuit has been built).
    built: bool,
    /// Whether live pricing was applied.
    priced: bool,
    /// Sum of extended prices when priced.
    total: Option<f64>,
    lines: Vec<BomLineDto>,
}

impl BomDto {
    fn not_built() -> Self {
        BomDto {
            built: false,
            priced: false,
            total: None,
            lines: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct BomLineDto {
    mpn: Option<String>,
    value: String,
    footprint: Option<String>,
    refdes: Vec<String>,
    qty: usize,
    unit_price: Option<f64>,
    ext_price: Option<f64>,
    image_url: Option<String>,
    /// The photo this line actually uses — curated, Thonk, or LCSC — resolved the
    /// same way the Visual BOM resolves it, so the dashboard crops the image the
    /// build will use rather than a different one. `None` unless `photos=true`.
    photo_src: Option<String>,
    /// The crop recorded for `photo_src`, as `[x, y, w, h]` fractions.
    crop: Option<[f64; 4]>,
}

fn default_suggest_limit() -> usize {
    3
}

#[derive(Deserialize)]
pub struct SuggestQuery {
    #[serde(default = "default_suggest_limit")]
    limit: usize,
}

#[derive(Serialize)]
struct SuggestDto {
    /// Whether a cached netlist was found (the circuit has been built).
    built: bool,
    /// Which distributor sources were consulted (`"mouser"`, `"lcsc"`); empty when
    /// none is available (no key + LCSC off).
    sources: Vec<String>,
    /// One entry per distinct generic part group (value + footprint).
    parts: Vec<SuggestPartDto>,
}

impl SuggestDto {
    fn not_built() -> Self {
        SuggestDto {
            built: false,
            sources: Vec::new(),
            parts: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct SuggestPartDto {
    /// The reference designators sharing this generic value/footprint.
    refdes: Vec<String>,
    value: String,
    footprint: Option<String>,
    /// The distributor keyword the search used (transparency for the UI).
    query: Option<String>,
    /// Ranked MPN candidates the human picks from (never auto-assigned).
    candidates: Vec<CandidateDto>,
}

#[derive(Serialize)]
struct CandidateDto {
    mpn: String,
    manufacturer: Option<String>,
    description: Option<String>,
    package: Option<String>,
    in_stock: Option<u64>,
    unit_price: Option<f64>,
    datasheet_url: Option<String>,
    image_url: Option<String>,
    source: String,
    lcsc_code: Option<String>,
    score: i64,
}

impl From<legion_of_bom_core::MpnCandidate> for CandidateDto {
    fn from(c: legion_of_bom_core::MpnCandidate) -> Self {
        CandidateDto {
            mpn: c.mpn,
            manufacturer: c.manufacturer,
            description: c.description,
            package: c.package,
            in_stock: c.in_stock,
            unit_price: c.unit_price,
            datasheet_url: c.datasheet_url,
            image_url: c.image_url,
            source: c.source.to_string(),
            lcsc_code: c.lcsc_code,
            score: c.score,
        }
    }
}

#[derive(Serialize)]
struct EditResult {
    /// The edit was written to `lob.toml`.
    written: bool,
    /// The file was staged (`git add`).
    staged: bool,
    /// Why staging didn't happen, if it didn't (the file is still written).
    git_error: Option<String>,
    /// Every repo-relative path currently staged (uncommitted) — the batch the
    /// human will commit.
    staged_files: Vec<String>,
}

#[derive(Serialize)]
struct OrdersDto {
    /// Whether the order store could be opened at all.
    available: bool,
    orders: Vec<OrderDto>,
}

impl OrdersDto {
    fn unavailable() -> Self {
        OrdersDto {
            available: false,
            orders: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct OrderDto {
    id: i64,
    module: String,
    vendor: Option<String>,
    status: String,
    ordered_at: Option<String>,
    tracking_ref: Option<String>,
    notes: Option<String>,
}

impl From<legion_of_bom_core::PanelOrder> for OrderDto {
    fn from(o: legion_of_bom_core::PanelOrder) -> Self {
        OrderDto {
            id: o.id,
            module: o.module,
            vendor: o.vendor,
            status: o.status.as_str().to_string(),
            ordered_at: o.ordered_at,
            tracking_ref: o.tracking_ref,
            notes: o.notes,
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// A path segment is unsafe if it could escape the artifact root.
fn is_unsafe(s: &str) -> bool {
    s.is_empty() || s.contains("..") || s.contains('\\') || s.contains('\0') || s.starts_with('/')
}

fn not_found(msg: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg }))).into_response()
}

fn repo_error(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": format!("circuits repo unavailable: {e}") })),
    )
        .into_response()
}

fn server_error(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg })),
    )
        .into_response()
}
