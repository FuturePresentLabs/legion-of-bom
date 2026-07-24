//! HTTP surface: health, the read API (projects, circuits, source, BOM, orders),
//! board and panel renders, the metadata-edit endpoint, static `/artifacts`
//! files, and the SPA fallback. State-shape comes from the shared core read
//! model; the web layer only orchestrates (render caching, staging) on top.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::api;
use crate::assets::serve_spa;
use crate::state::AppState;

/// Assemble the router: `/api/*` JSON endpoints, `/artifacts/*` static files from
/// the `out/` tree, everything else the SPA.
pub fn build_router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/repo", get(api::repo))
        .route("/circuits", get(api::circuits))
        .route("/circuits/{name}", get(api::circuit))
        .route("/circuits/{name}/source", get(api::source))
        .route("/circuits/{name}/bom", get(api::bom))
        .route("/circuits/{name}/suggest", get(api::suggest))
        .route("/circuits/{name}/orders", get(api::orders))
        .route("/circuits/{name}/render", get(crate::render::render))
        .route("/circuits/{name}/sim", post(crate::sim::sim))
        .route("/edit", post(api::edit));
    Router::new()
        .nest("/api", api)
        .route("/artifacts/{name}/{*path}", get(api::artifact))
        .fallback(serve_spa)
        .with_state(state)
}

/// Liveness plus a one-line summary of the repo being served.
#[derive(Serialize)]
struct Health {
    /// `ok` when a circuits repo resolved, `no-repo` otherwise.
    status: &'static str,
    /// The repo name from `lob.toml`, if any.
    repo: Option<String>,
    /// Number of circuits declared.
    circuits: usize,
}

/// `GET /api/health` — proves the axum head is wired to the shared core read
/// model by reporting the resolved repo + circuit count.
async fn health(State(state): State<Arc<AppState>>) -> Json<Health> {
    match state.project() {
        Ok(view) => Json(Health {
            status: "ok",
            repo: view.repo.name.clone(),
            circuits: view.circuits.len(),
        }),
        Err(_) => Json(Health {
            status: "no-repo",
            repo: None,
            circuits: 0,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// A throwaway circuits repo, cleaned up on drop.
    struct TempRepo(std::path::PathBuf);
    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_repo(tag: &str, body: &str) -> TempRepo {
        let dir = std::env::temp_dir().join(format!(
            "lob-web-test-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lob.toml"), body).unwrap();
        TempRepo(dir)
    }

    #[tokio::test]
    async fn health_reports_repo_and_circuit_count() {
        let repo = temp_repo(
            "ok",
            "[repo]\nname = \"t\"\n[[circuit]]\nname = \"c\"\nsource = \"c.py\"\n",
        );
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["repo"], "t");
        assert_eq!(v["circuits"], 1);
    }

    #[tokio::test]
    async fn health_degrades_when_no_repo() {
        // An empty dir with no lob.toml (and none above it in temp) → no-repo.
        let dir = std::env::temp_dir().join(format!(
            "lob-web-norepo-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = build_router(Arc::new(AppState::new(dir.clone())));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "no-repo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GET `uri` against a router, returning (status, parsed-JSON-or-Null).
    async fn get(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn one_circuit_repo(tag: &str) -> TempRepo {
        temp_repo(
            tag,
            "[repo]\nname = \"puget\"\nbrand = \"Puget Audio\"\n\
             [[circuit]]\nname = \"slew\"\nsource = \"slew.py\"\n",
        )
    }

    #[tokio::test]
    async fn repo_endpoint_reports_masthead_and_count() {
        let repo = one_circuit_repo("repo");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = get(app, "/api/repo").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["name"], "puget");
        assert_eq!(v["brand"], "Puget Audio");
        assert_eq!(v["circuits"], 1);
    }

    #[tokio::test]
    async fn circuits_list_and_detail() {
        let repo = one_circuit_repo("detail");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = get(app.clone(), "/api/circuits").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v[0]["name"], "slew");
        // Detail carries the artifact inventory.
        let (status, v) = get(app.clone(), "/api/circuits/slew").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["name"], "slew");
        assert!(v["artifacts"].is_array());
        // Unknown circuit → 404.
        let (status, _) = get(app, "/api/circuits/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn bom_reports_not_built_without_netlist() {
        let repo = one_circuit_repo("bom");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = get(app, "/api/circuits/slew/bom").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["built"], false);
        assert_eq!(v["priced"], false);
        assert_eq!(v["lines"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn suggest_reports_not_built_without_netlist() {
        // No cached netlist → built:false, no network hit, no candidates. Keeps
        // this test hermetic (the built path would call Mouser/LCSC live).
        let repo = one_circuit_repo("suggest");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = get(app.clone(), "/api/circuits/slew/suggest").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["built"], false);
        assert_eq!(v["parts"].as_array().unwrap().len(), 0);
        // Unknown circuit → 404.
        let (status, _) = get(app, "/api/circuits/nope/suggest").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn artifact_serves_file_and_blocks_traversal() {
        let repo = one_circuit_repo("artifact");
        std::fs::create_dir_all(repo.0.join("out/slew")).unwrap();
        std::fs::write(repo.0.join("out/slew/slew-guide.html"), "<h1>guide</h1>").unwrap();
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/artifacts/slew/slew-guide.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"<h1>guide</h1>");

        // Traversal in the wildcard tail is rejected.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/artifacts/slew/..%2f..%2flob.toml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// POST `uri` with a JSON body, returning (status, parsed-JSON).
    async fn post(
        app: Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn edit_writes_metadata_even_without_git() {
        let repo = one_circuit_repo("edit");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = post(
            app,
            "/api/edit",
            serde_json::json!({
                "circuits": [{ "name": "slew", "build_intro": "Edited intro." }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["written"], true);
        // The temp dir is not a git repo, so staging is reported as not done —
        // but the file is written regardless.
        assert_eq!(v["staged"], false);
        assert!(v["git_error"].is_string());
        let toml = std::fs::read_to_string(repo.0.join("lob.toml")).unwrap();
        assert!(toml.contains("Edited intro."));
    }

    #[tokio::test]
    async fn edit_rejects_empty_and_unknown_circuit() {
        let repo = one_circuit_repo("edit-bad");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, _) = post(app.clone(), "/api/edit", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = post(
            app,
            "/api/edit",
            serde_json::json!({ "circuits": [{ "name": "nope", "build_intro": "x" }] }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn render_error_paths_resolve_before_kicad() {
        // The temp repo's circuit has no built board and no panel — every error
        // path is reachable without kicad-cli.
        let repo = one_circuit_repo("render");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let cases = [
            (
                "/api/circuits/slew/render?view=board-top",
                StatusCode::NOT_FOUND,
            ), // not built
            (
                "/api/circuits/slew/render?view=board-layout",
                StatusCode::NOT_FOUND,
            ), // not built (svg layout)
            (
                "/api/circuits/slew/render?view=panel",
                StatusCode::NOT_FOUND,
            ), // no panel
            (
                "/api/circuits/slew/render?view=bogus",
                StatusCode::BAD_REQUEST,
            ), // bad view
            (
                "/api/circuits/nope/render?view=board-top",
                StatusCode::NOT_FOUND,
            ), // no circuit
        ];
        for (uri, want) in cases {
            assert_eq!(get(app.clone(), uri).await.0, want, "{uri}");
        }
    }

    #[tokio::test]
    async fn source_serves_python_and_404s() {
        let repo = one_circuit_repo("source");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        // Source declared but not yet on disk → 404.
        assert_eq!(
            get(app.clone(), "/api/circuits/slew/source").await.0,
            StatusCode::NOT_FOUND
        );
        // Write it, and it serves with the python hint.
        std::fs::write(repo.0.join("slew.py"), "from skidl import *\n# hi\n").unwrap();
        let (status, v) = get(app.clone(), "/api/circuits/slew/source").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["language"], "python");
        assert_eq!(v["path"], "slew.py");
        assert!(v["content"].as_str().unwrap().contains("skidl"));
        // Unknown circuit → 404.
        assert_eq!(
            get(app, "/api/circuits/nope/source").await.0,
            StatusCode::NOT_FOUND
        );
    }
}
