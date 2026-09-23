//! On-demand transient simulation for the dashboard scope (5hr).
//!
//! `POST /api/circuits/{name}/sim` drives the circuit's input net with a caller-
//! supplied piecewise-linear stimulus (a 1V/oct sequence, an LFO, …), runs an
//! ngspice `.tran`, and returns the probed output waveform — so the front end can
//! play CV into a module and scope the response. Reads the cached netlist
//! (`out/<name>/<name>.net`), so no SKiDL run; ngspice must be on PATH.

use std::path::Path as FsPath;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use legion_of_bom_core::{
    parse_netlist_file, simulate_tran_drive, SimConfig, StageError, TranDrive, TranPoint,
};

use crate::state::AppState;

/// A scope request: the input stimulus + timing + which net to probe.
#[derive(Debug, Deserialize)]
pub struct SimRequest {
    /// Input-source breakpoints `[t_s, volts]`, ascending in time.
    pub pwl: Vec<[f64; 2]>,
    /// `.tran` timestep (s). Clamped to a sane range.
    pub step_s: f64,
    /// `.tran` stop time (s). Clamped to a sane range.
    pub stop_s: f64,
    /// Extra forced control nets: `{net, pwl:[[t,v]…]}` (e.g. drive `RATE_CV` to
    /// set/modulate the slew rate). Optional.
    #[serde(default)]
    pub cv: Vec<CvDrive>,
    /// Net to probe; `None` → the auto-detected output net.
    #[serde(default)]
    pub probe: Option<String>,
}

/// One forced control net + its PWL breakpoints.
#[derive(Debug, Deserialize)]
pub struct CvDrive {
    pub net: String,
    pub pwl: Vec<[f64; 2]>,
}

/// The scope response: resolved nets + the probed waveform.
#[derive(Debug, Serialize)]
pub struct SimResponse {
    pub input_net: String,
    pub probe_net: String,
    pub stop_s: f64,
    pub output: Vec<TranPoint>,
}

/// Guard rails so a bad request can't spawn a pathological ngspice run.
const MAX_PWL_POINTS: usize = 512;
const MIN_STEP_S: f64 = 1e-6;
const MAX_STOP_S: f64 = 60.0;

/// `POST /api/circuits/{name}/sim` — run a driven transient and return the trace.
pub async fn sim(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<SimRequest>,
) -> Response {
    // The circuit must exist in the manifest.
    match state.project() {
        Ok(v) if v.circuit(&name).is_none() => {
            return err(StatusCode::NOT_FOUND, &format!("no circuit '{name}'"));
        }
        Ok(_) => {}
        Err(e) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("repo unavailable: {e}"),
            )
        }
    }

    if req.pwl.len() < 2 {
        return err(StatusCode::BAD_REQUEST, "pwl needs at least two points");
    }
    if req.pwl.len() > MAX_PWL_POINTS {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("pwl has too many points (max {MAX_PWL_POINTS})"),
        );
    }
    let step_s = req.step_s.clamp(MIN_STEP_S, 1.0);
    let stop_s = req.stop_s.clamp(step_s * 10.0, MAX_STOP_S);

    let root = state.root().to_path_buf();
    let drive = TranDrive {
        step_s,
        stop_s,
        pwl: req.pwl.iter().map(|p| (p[0], p[1])).collect(),
        cv: req
            .cv
            .iter()
            .map(|c| (c.net.clone(), c.pwl.iter().map(|p| (p[0], p[1])).collect()))
            .collect(),
        probe_net: req.probe.clone(),
    };

    match tokio::task::spawn_blocking(move || run_sim(&root, &name, drive)).await {
        Ok(Ok(resp)) => Json(resp).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "sim task panicked"),
    }
}

/// Parse the cached netlist, infer the harness, run the driven transient.
fn run_sim(root: &FsPath, name: &str, drive: TranDrive) -> Result<SimResponse, SimErr> {
    let netlist = root.join("out").join(name).join(format!("{name}.net"));
    let model = parse_netlist_file(&netlist).map_err(|_| SimErr::NotBuilt(name.to_string()))?;
    let config = SimConfig::infer(&model);
    let probe_net = drive
        .probe_net
        .clone()
        .unwrap_or_else(|| config.output_net.clone());

    let work_dir = root.join("out").join(name);
    let result = simulate_tran_drive(&model, &config, &drive, &work_dir).map_err(SimErr::Stage)?;
    Ok(SimResponse {
        input_net: config.input_net,
        probe_net,
        stop_s: drive.stop_s,
        output: result.points,
    })
}

/// A sim failure mapped to a status the dashboard can act on.
enum SimErr {
    NotBuilt(String),
    Stage(StageError),
}

impl IntoResponse for SimErr {
    fn into_response(self) -> Response {
        match self {
            SimErr::NotBuilt(name) => err(
                StatusCode::NOT_FOUND,
                &format!("circuit not built — run `lob build {name}`"),
            ),
            SimErr::Stage(StageError::ToolNotFound(_)) => err(
                StatusCode::SERVICE_UNAVAILABLE,
                "ngspice not found — install ngspice to use the scope",
            ),
            SimErr::Stage(e) => err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("sim failed: {e}"),
            ),
        }
    }
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}
