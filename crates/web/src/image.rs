//! Part-photo bytes, and the crop chosen for them.
//!
//! Shop photos are shot for shops: a Thonk listing for jack sockets is a
//! photograph of a *handful* of jack sockets, which makes a poor 13mm cell in a
//! Visual BOM. This is the surface the dashboard's crop editor drives — read the
//! full photo, record a rectangle over it, and every downstream render (Visual
//! BOM, build guide) picks it up, because the crop is stored beside the cached
//! image and applied by [`legion_of_bom_core::embed_source`].
//!
//! Serving is **cache-only** on purpose. An endpoint that fetched whatever URL a
//! request named would turn the dashboard into a proxy for arbitrary outbound
//! GETs; the crop editor only ever works on photos the BOM already resolved and
//! cached, so this costs nothing real.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use legion_of_bom_core::{
    cached_source_bytes, default_image_cache_dir, source_mime, write_crop, Crop,
};

use crate::state::AppState;

#[derive(Deserialize)]
pub struct ImageQuery {
    /// The image source: an http(s) URL or a `file://` path, exactly as the BOM
    /// endpoint reported it.
    src: String,
    /// Serve the original rather than the cropped result — what the editor shows
    /// while you are choosing the rectangle.
    #[serde(default)]
    raw: bool,
}

/// `GET /api/image?src=…&raw=…` — a part photo's bytes, cropped unless `raw`.
pub async fn image(State(_state): State<Arc<AppState>>, Query(q): Query<ImageQuery>) -> Response {
    let cache = default_image_cache_dir();
    let Some(bytes) = cached_source_bytes(&q.src, &cache) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no cached image for that source" })),
        )
            .into_response();
    };
    let mime = source_mime(&q.src);
    // `cropped_bytes` re-reads from the same cache entry, so this cannot reach the
    // network; it falls back to the full photo if the crop cannot be applied.
    let bytes = match q.raw {
        true => bytes,
        false => legion_of_bom_core::cropped_bytes(&q.src, &cache)
            .map(|(b, _)| b)
            .unwrap_or(bytes),
    };
    (
        StatusCode::OK,
        // The bytes change when the crop does, and the URL does not, so this must
        // not be cached by the browser or a save would appear to do nothing.
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct CropBody {
    src: String,
    /// The rectangle as fractions of the source image; `None` clears the crop and
    /// restores the full photo.
    crop: Option<[f64; 4]>,
}

/// `POST /api/image/crop` — record (or clear) the crop for a photo.
pub async fn crop(State(_state): State<Arc<AppState>>, Json(body): Json<CropBody>) -> Response {
    let cache = default_image_cache_dir();
    // Refuse a source we have never seen, for the same reason serving is
    // cache-only: this endpoint should not be a way to write arbitrary files.
    if cached_source_bytes(&body.src, &cache).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no cached image for that source" })),
        )
            .into_response();
    }
    let crop = body.crop.map(|[x, y, w, h]| Crop { x, y, w, h });
    if let Some(c) = crop {
        if !c.is_sane() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "crop must lie inside the image and be non-empty" })),
            )
                .into_response();
        }
    }
    match write_crop(&cache, &body.src, crop) {
        Ok(()) => Json(json!({ "ok": true, "cropped": crop.is_some() })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("saving crop: {e}") })),
        )
            .into_response(),
    }
}
