//! SPA asset serving.
//!
//! Release/packaged builds embed `web/dist` into the binary via `rust-embed`
//! (feature `embed-assets`). Dev builds serve `web/dist` from the crate source
//! so the front end can be rebuilt (`cd crates/web/web && npm run build`) without
//! recompiling Rust. Lifted from Understory's `assets.rs`.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

#[cfg(feature = "embed-assets")]
#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
struct Assets;

pub(crate) fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("map") | Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Canonicalize a request path into a safe asset key, or fall back to the SPA's
/// `index.html`. Traversal segments are rejected outright.
fn asset_key(uri: &Uri) -> Option<String> {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return Some("index.html".to_string());
    }
    if path.contains("..") || path.contains('\\') || path.contains('\0') {
        return None;
    }
    Some(path.to_string())
}

/// The SPA fallback: serve the requested asset, or `index.html` for unknown
/// non-asset paths so client-side routing works.
pub async fn serve_spa(uri: Uri) -> Response {
    let Some(key) = asset_key(&uri) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    match load(&key) {
        Some(bytes) => asset_response(&key, bytes),
        // SPA routing: unknown non-asset paths get index.html.
        None if !key.contains('.') => match load("index.html") {
            Some(bytes) => asset_response("index.html", bytes),
            None => spa_missing(),
        },
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn asset_response(key: &str, bytes: Vec<u8>) -> Response {
    let cache = if key.starts_with("assets/") {
        // Vite emits content-hashed filenames under assets/.
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, content_type(key)),
            (header::CACHE_CONTROL, cache),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

fn spa_missing() -> Response {
    (
        StatusCode::NOT_FOUND,
        "SPA assets not found. Build the front end (cd crates/web/web && npm run build) \
         or use a binary built with --features embed-assets.",
    )
        .into_response()
}

#[cfg(feature = "embed-assets")]
fn load(key: &str) -> Option<Vec<u8>> {
    Assets::get(key).map(|f| f.data.into_owned())
}

#[cfg(not(feature = "embed-assets"))]
fn load(key: &str) -> Option<Vec<u8>> {
    // Dev fallback: read from web/dist in the crate source, independent of the
    // process's working directory (`lob serve` runs from a circuits repo).
    let base = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/web/dist"));
    let path = base.join(key);
    // asset_key already rejected traversal; double-check containment anyway.
    if !path.starts_with(base) {
        return None;
    }
    std::fs::read(path).ok()
}
