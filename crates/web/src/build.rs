//! Build a circuit from the dashboard (ef6).
//!
//! `POST /api/circuits/{name}/build` runs the same `lob build <name>` the CLI
//! runs — by invoking this very executable rather than reimplementing the
//! pipeline, so the dashboard can never drift from the command line.
//!
//! The whole transcript comes back on success *and* on failure. A build that
//! fails does so for a reason the user needs to read (a missing footprint, a
//! tool not on PATH), and hiding it behind "build failed" would make the button
//! useless precisely when it matters.

use std::process::Command;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct BuildResult {
    /// Whether the build completed every stage.
    pub ok: bool,
    /// The build transcript, stdout and stderr interleaved as the CLI prints it.
    pub output: String,
}

pub async fn build(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match state.project() {
        Ok(view) if view.circuit(&name).is_none() => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("no circuit '{name}'") })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    let root = state.root().to_path_buf();
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("locating the lob binary: {e}") })),
            )
                .into_response();
        }
    };

    // A build shells out to KiCad and can run for a while — keep it off the
    // async runtime.
    let done = tokio::task::spawn_blocking(move || {
        Command::new(exe)
            .arg("build")
            .arg(&name)
            .current_dir(&root)
            .output()
    })
    .await;

    match done {
        Ok(Ok(out)) => {
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                output.push_str(&stderr);
            }
            Json(BuildResult {
                ok: out.status.success(),
                output,
            })
            .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("running lob build: {e}") })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "build task panicked" })),
        )
            .into_response(),
    }
}
