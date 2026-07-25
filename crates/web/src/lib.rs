//! `legion-of-bom-web` — the localhost dashboard backend (epic p58).
//!
//! The third head (DESIGN 2.2) alongside the CLI and the MCP agent: an axum
//! server that wraps the SAME `legion-of-bom-core`. Every page is a view over the
//! shared [`legion_of_bom_core::ProjectView`] read model — never a web-only
//! reimplementation, so anything the dashboard shows the CLI can show headless.
//! Local-first and single-user: it binds localhost with NO auth layer at all
//! (DESIGN 2.5); multi-tenant / remote-repo is Phase 5.
//!
//! Stack mirrors Understory: axum + a Preact/Vite SPA embedded via `rust-embed`
//! behind the `embed-assets` feature (dev builds serve `web/dist` from disk).

pub mod api;
pub mod assets;
pub mod build;
pub mod render;
pub mod routes;
mod sim;
pub mod state;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

pub use routes::build_router;
pub use state::AppState;

/// Serve the dashboard for the circuits repo rooted at `root`, bound to `addr`,
/// until the process is stopped. Async — call it from inside a runtime, or use
/// [`serve_blocking`] from synchronous code.
pub async fn serve(root: PathBuf, addr: SocketAddr) -> anyhow::Result<()> {
    let state = Arc::new(AppState::new(root));
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "legion-of-bom dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// [`serve`], but builds a Tokio runtime and blocks on it — the entry point the
/// synchronous CLI (`lob serve`) calls.
pub fn serve_blocking(root: PathBuf, addr: SocketAddr) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    runtime.block_on(serve(root, addr))
}
