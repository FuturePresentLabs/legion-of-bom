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
        .route("/circuits/{name}/sides", get(crate::render::sides))
        .route("/circuits/{name}/rules", get(crate::render::rules))
        .route(
            "/circuits/{name}/placement",
            get(crate::placement::get).post(crate::placement::post),
        )
        .route("/circuits/{name}/controls", get(crate::placement::controls))
        .route("/circuits/{name}/sim", post(crate::sim::sim))
        .route("/circuits/{name}/build", post(crate::build::build))
        .route("/image", get(crate::image::image))
        .route("/image/crop", post(crate::image::crop))
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

    /// A netlist with the two jacks and the pot the placement tests move around.
    /// Structurally faithful to what SKiDL emits — the write guard reads its refs.
    const PANEL_NETLIST: &str = r#"
    (export (version "E")
      (design (source "slew.py"))
      (components
        (comp (ref "J1") (value "Thonkiconn"))
        (comp (ref "J2") (value "Thonkiconn"))
        (comp (ref "RV1") (value "100k")))
      (nets
        (net (code 1) (name "IN") (class "Default")
          (node (ref "J1") (pin "1") (pintype "PASSIVE")))))
    "#;

    /// Give the repo a cached netlist, which is what "built" means to the
    /// placement guard.
    fn build_netlist(repo: &TempRepo) {
        std::fs::create_dir_all(repo.0.join("out/slew")).unwrap();
        std::fs::write(repo.0.join("out/slew/slew.net"), PANEL_NETLIST).unwrap();
    }

    #[tokio::test]
    async fn placement_reads_as_empty_before_anything_is_placed() {
        let repo = one_circuit_repo("placement-empty");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = get(app.clone(), "/api/circuits/slew/placement").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["exists"], false);
        // The path is reported even when absent — that is where a first edit goes.
        assert_eq!(v["path"], "slew.placement.toml");
        assert!(v["positions"].as_object().unwrap().is_empty());
        // Unknown circuit → 404.
        assert_eq!(
            get(app, "/api/circuits/nope/placement").await.0,
            StatusCode::NOT_FOUND
        );
    }

    /// The write guard has nothing to check against until the circuit is built,
    /// so the endpoint refuses rather than writing an unchecked placement.
    #[tokio::test]
    async fn placement_write_refuses_an_unbuilt_circuit() {
        let repo = one_circuit_repo("placement-unbuilt");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = post(
            app,
            "/api/circuits/slew/placement",
            serde_json::json!({
                "ops": [{ "op": "set_control", "refdes": "J1", "x": 6.0, "y": 10.5 }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(v["error"].as_str().unwrap().contains("has not been built"));
        assert!(!repo.0.join("slew.placement.toml").exists());
    }

    #[tokio::test]
    async fn placement_write_creates_the_file_and_reads_back() {
        let repo = one_circuit_repo("placement-write");
        build_netlist(&repo);
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));

        // "Make these two jacks a column on 13.5mm pitch."
        let (status, v) = post(
            app.clone(),
            "/api/circuits/slew/placement",
            serde_json::json!({
                "ops": [{
                    "op": "set_column",
                    "refdes": ["J1", "J2"],
                    "x": 6.0, "from_y": 10.5, "pitch": 13.5
                }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["written"], true);
        assert_eq!(v["path"], "slew.placement.toml");
        assert_eq!(v["positions"]["J2"]["y"], 24.0);
        // Not a git repo, so staging is reported as not done — the file lands anyway.
        assert_eq!(v["staged"], false);

        // Intent survives: one pattern table, not two coordinates.
        let toml = std::fs::read_to_string(repo.0.join("slew.placement.toml")).unwrap();
        assert!(toml.contains("[[patterns.column]]"), "{toml}");
        assert!(!toml.contains("[controls]"), "{toml}");

        let (status, v) = get(app, "/api/circuits/slew/placement").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["exists"], true);
        assert_eq!(v["file"]["patterns"]["column"][0]["pitch"], 13.5);
        assert_eq!(v["positions"]["J1"]["x"], 6.0);
    }

    #[tokio::test]
    async fn placement_write_rejects_a_part_the_circuit_does_not_have() {
        let repo = one_circuit_repo("placement-guard");
        build_netlist(&repo);
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = post(
            app.clone(),
            "/api/circuits/slew/placement",
            serde_json::json!({
                "ops": [{ "op": "set_control", "refdes": "J9", "x": 1.0, "y": 1.0 }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("J9"));
        assert!(!repo.0.join("slew.placement.toml").exists());
        // An edit with no ops is a client mistake, not an empty write.
        let (status, _) = post(
            app,
            "/api/circuits/slew/placement",
            serde_json::json!({ "ops": [] }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Reshaping a strip moves the parts that were not dragged. The endpoint says
    /// so rather than letting the board change under the user.
    #[tokio::test]
    async fn placement_write_reports_collateral_movement() {
        let repo = one_circuit_repo("placement-side-effects");
        build_netlist(&repo);
        std::fs::write(
            repo.0.join("slew.placement.toml"),
            "[[patterns.column]]\nrefdes = [\"J1\", \"J2\"]\nx = 6.0\nfrom_y = 10.5\npitch = 13.5\n",
        )
        .unwrap();
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = post(
            app,
            "/api/circuits/slew/placement",
            serde_json::json!({
                "ops": [{ "op": "set_control", "refdes": "J1", "x": 20.0, "y": 90.0 }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        // The override does not disturb the strip…
        assert!(v["side_effects"].as_array().unwrap().is_empty());
        assert_eq!(v["positions"]["J2"]["y"], 24.0);

        // …but dropping a member does, and that is reported.
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = post(
            app,
            "/api/circuits/slew/placement",
            serde_json::json!({
                "ops": [
                    { "op": "clear_control", "refdes": "J1" },
                    { "op": "drop_from_pattern", "refdes": "J1" }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["positions"]["J2"]["y"], 10.5);
        // J2 was not named in either op, yet it moved a whole pitch.
        assert_eq!(v["side_effects"][0]["refdes"], "J2");
        assert_eq!(v["side_effects"][0]["from"]["y"], 24.0);
        assert_eq!(v["side_effects"][0]["to"]["y"], 10.5);
    }

    /// A 5 HP board: a jack low, a pot high, and one 0603 that is the placer's
    /// business and nobody else's.
    const PANEL_BOARD: &str = r#"(kicad_pcb
      (gr_rect (start 100 40) (end 125.4 168.5) (layer "Edge.Cuts"))
      (footprint "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical" (layer "F.Cu") (at 106 158 0)
        (property "Reference" "J1") (pad "1" thru_hole circle (at 0 0) (size 2 2)))
      (footprint "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical" (layer "F.Cu") (at 118 55 0)
        (property "Reference" "RV1") (pad "1" thru_hole circle (at 0 0) (size 2 2)))
      (footprint "Resistor_SMD:R_0603_1608Metric" (layer "F.Cu") (at 110 100 0)
        (property "Reference" "R1") (pad "1" smd rect (at 0 0) (size 1 1))))"#;

    fn build_board(repo: &TempRepo) {
        std::fs::create_dir_all(repo.0.join("out/slew")).unwrap();
        std::fs::write(repo.0.join("out/slew/slew.kicad_pcb"), PANEL_BOARD).unwrap();
    }

    #[tokio::test]
    async fn controls_report_the_board_in_panel_space() {
        let repo = one_circuit_repo("controls");
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        // Nothing built → said plainly, not an error and not an empty panel that
        // would read as a placement decision.
        let (status, v) = get(app.clone(), "/api/circuits/slew/controls").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["built"], false);

        build_netlist(&repo);
        build_board(&repo);
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = get(app.clone(), "/api/circuits/slew/controls").await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["built"], true);
        assert_eq!(v["hp"], 5);
        let parts = v["parts"].as_array().unwrap();
        let by = |r: &str| {
            parts
                .iter()
                .find(|p| p["refdes"] == r)
                .unwrap_or_else(|| panic!("no {r}"))
        };
        // J1 sits 6mm across and 10.5mm up from the bottom of the panel.
        assert_eq!(by("J1")["x"], 6.0);
        assert_eq!(by("J1")["y"], 10.5);
        assert_eq!(by("J1")["panel"], true);
        assert_eq!(by("J1")["kind"], "jack");
        // The 0603 is context, drawn dimmed and not movable.
        assert_eq!(by("R1")["panel"], false);
        assert!(by("R1")["kind"].is_null());
        // A knob is bigger than its hole, and that is what fouls its neighbour.
        assert!(by("RV1")["envelope_mm"][0].as_f64().unwrap() > 7.0);

        assert_eq!(
            get(app, "/api/circuits/nope/controls").await.0,
            StatusCode::NOT_FOUND
        );
    }

    /// A drag posts where parts should end up; the server decides which patterns
    /// survive. Moving a whole strip must keep it a strip.
    #[tokio::test]
    async fn a_targets_edit_keeps_the_pattern_that_still_fits() {
        let repo = one_circuit_repo("targets");
        build_netlist(&repo);
        std::fs::write(
            repo.0.join("slew.placement.toml"),
            "# the jack strip\n[[patterns.column]]\nrefdes = [\"J1\", \"J2\"]\nx = 6.0\nfrom_y = 10.5\npitch = 13.5\n",
        )
        .unwrap();
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = post(
            app,
            "/api/circuits/slew/placement",
            serde_json::json!({
                "targets": { "J1": { "x": 9.0, "y": 10.5 }, "J2": { "x": 9.0, "y": 24.0 } }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["written"], true);
        assert_eq!(v["positions"]["J2"]["x"], 9.0);
        let toml = std::fs::read_to_string(repo.0.join("slew.placement.toml")).unwrap();
        // Still one pattern, still commented — not two coordinates sharing an x.
        assert_eq!(toml.matches("[[patterns.column]]").count(), 1, "{toml}");
        assert!(toml.contains("# the jack strip"), "{toml}");
        assert!(!toml.contains("[controls]"), "{toml}");
    }

    /// Undo: ask for the previous file back and get it, exactly.
    #[tokio::test]
    async fn a_restore_edit_puts_the_file_back() {
        let repo = one_circuit_repo("restore");
        build_netlist(&repo);
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));

        let (_, before) = post(
            app.clone(),
            "/api/circuits/slew/placement",
            serde_json::json!({
                "ops": [{
                    "op": "set_column",
                    "refdes": ["J1", "J2"], "x": 6.0, "from_y": 10.5, "pitch": 13.5
                }]
            }),
        )
        .await;
        // Something destructive, then undo it.
        let (status, _) = post(
            app.clone(),
            "/api/circuits/slew/placement",
            serde_json::json!({
                "targets": { "J1": { "x": 2.0, "y": 90.0 }, "J2": { "x": 20.0, "y": 90.0 } }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, v) = post(
            app,
            "/api/circuits/slew/placement",
            serde_json::json!({ "restore": before["file"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["file"], before["file"], "undo did not restore the intent");
        assert_eq!(v["positions"], before["positions"]);
    }

    /// Restoring what the file already says is success with nothing to do — not
    /// the empty-edit error a caller sending `ops: []` earns.
    #[tokio::test]
    async fn a_no_op_restore_reports_nothing_written() {
        let repo = one_circuit_repo("restore-noop");
        build_netlist(&repo);
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = post(
            app,
            "/api/circuits/slew/placement",
            serde_json::json!({ "restore": { "controls": {}, "patterns": {} } }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["written"], false);
        assert!(!repo.0.join("slew.placement.toml").exists());
    }

    #[tokio::test]
    async fn two_edit_forms_in_one_request_are_refused() {
        let repo = one_circuit_repo("two-forms");
        build_netlist(&repo);
        let app = build_router(Arc::new(AppState::new(repo.0.clone())));
        let (status, v) = post(
            app,
            "/api/circuits/slew/placement",
            serde_json::json!({
                "ops": [{ "op": "clear_control", "refdes": "J1" }],
                "targets": { "J1": { "x": 1.0, "y": 1.0 } }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("exactly one"));
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
