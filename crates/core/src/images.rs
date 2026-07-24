//! Fetch, cache, and embed small part photos for the Visual BOM. 5uj.4.
//!
//! Source-agnostic: given any image URL (from a part record, a distributor API,
//! or a hand-curated entry) it downloads the image once, caches it on disk by URL
//! hash, and returns it as an embeddable `data:` URI so the Visual BOM stays a
//! single self-contained file. Every failure path returns `None` and the caller
//! falls back to a color swatch / blank cell — a photo can never break the BOM.
//!
//! The working photo source is EasyEDA/LCSC ([`crate::easyeda`]), which resolves
//! an MPN or distinctive value to a hotlinkable product photo. The obvious
//! distributor routes do *not* work server-side (verified 2026-07-24): Mouser's
//! `ImagePath` host **bot-blocks** GETs (200 with an HTML "access denied" page)
//! and JLCPCB's component API exposes no image — so [`download`] magic-byte-
//! validates every payload and rejects non-images, ensuring a block page can
//! never be embedded as a broken photo. Any plain image URL works here too (a
//! curated `image_url`, a keyed distributor API), fed in as a URL.

use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use sha2::{Digest, Sha256};

/// Cap on a fetched image (Mouser thumbnails are a few KB; this guards against a
/// surprise multi-MB payload bloating the embedded HTML).
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// The MIME type for a data URI, from the URL extension (Mouser serves JPEG, the
/// default). Query strings are ignored.
fn mime_from_url(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "image/jpeg"
    }
}

/// A self-contained `data:` URI embedding `bytes` with `mime`.
pub fn to_data_uri(bytes: &[u8], mime: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:{mime};base64,{b64}")
}

/// The on-disk cache path for a URL: `<cache_dir>/<sha256(url)>.<ext>`.
fn cache_path(cache_dir: &Path, url: &str) -> PathBuf {
    let hash: String = Sha256::digest(url.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    // `image/jpeg` → `jpeg`, `image/svg+xml` → `svg+xml`; fine for a cache key.
    let ext = mime_from_url(url).rsplit('/').next().unwrap_or("jpeg");
    cache_dir.join(format!("{hash}.{ext}"))
}

/// Fetch an image by URL and return it as an embeddable `data:` URI, caching the
/// bytes under `cache_dir`. A cached file is reused without any network call
/// (offline-friendly). Returns `None` on any failure — callers degrade, never fail.
pub fn fetch_data_uri(url: &str, cache_dir: &Path) -> Option<String> {
    let mime = mime_from_url(url);
    let path = cache_path(cache_dir, url);
    // A cached file is trusted only if it still looks like an image (guards against
    // a stale cache written before payload validation existed).
    if let Ok(bytes) = std::fs::read(&path) {
        if is_probably_image(&bytes) {
            return Some(to_data_uri(&bytes, mime));
        }
    }
    let bytes = download(url)?;
    let _ = std::fs::create_dir_all(cache_dir);
    let _ = std::fs::write(&path, &bytes);
    Some(to_data_uri(&bytes, mime))
}

/// Embed an image *source* — either a `file://` local path (a hand-attached
/// photo) or an http(s) URL (a distributor/curated image) — as a `data:` URI.
/// Local files are read + validated directly; URLs go through [`fetch_data_uri`]
/// (fetch + cache). `None` on any failure, so callers degrade to a swatch/blank.
pub fn embed_source(source: &str, cache_dir: &Path) -> Option<String> {
    if let Some(path) = source.strip_prefix("file://") {
        let bytes = std::fs::read(path).ok()?;
        return is_probably_image(&bytes).then(|| to_data_uri(&bytes, mime_from_url(path)));
    }
    fetch_data_uri(source, cache_dir)
}

/// GET `url` into bytes, bounded by [`MAX_BYTES`]; `None` on any error or if the
/// payload isn't a real image. The last check matters: distributor image hosts
/// (e.g. Mouser) bot-block server-side fetches and return an *HTML* "access
/// denied" page with a 200 status — embedding that as a JPEG would show a broken
/// image, so a non-image body is treated as failure and the caller falls back.
fn download(url: &str) -> Option<Vec<u8>> {
    let resp = ureq::get(url)
        .set(
            "User-Agent",
            "Mozilla/5.0 (compatible; legion-of-bom Visual BOM)",
        )
        .call()
        .ok()?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_BYTES && is_probably_image(&bytes)).then_some(bytes)
}

/// Whether `bytes` begin with a known raster-image signature (JPEG, PNG, GIF,
/// WebP, BMP). Rejects HTML/text so a bot-block page is never mistaken for a photo.
fn is_probably_image(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8, 0xFF])                              // JPEG
        || b.starts_with(b"\x89PNG\r\n\x1a\n")                      // PNG
        || b.starts_with(b"GIF87a")
        || b.starts_with(b"GIF89a")                                // GIF
        || (b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP") // WebP
        || b.starts_with(b"BM") // BMP
}

/// The shared, cross-project image cache directory (override with `LOB_IMAGE_CACHE`).
/// Photos keyed by URL are reused across every project that references the part.
pub fn default_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOB_IMAGE_CACHE") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("legion-of-bom").join("images")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_and_data_uri() {
        assert_eq!(mime_from_url("https://x/y.PNG"), "image/png");
        assert_eq!(mime_from_url("https://x/y.jpg?rev=2"), "image/jpeg");
        assert_eq!(mime_from_url("https://x/thumb"), "image/jpeg"); // default
        assert_eq!(
            to_data_uri(b"hi", "image/png"),
            "data:image/png;base64,aGk="
        );
    }

    #[test]
    fn cache_path_is_deterministic_and_extensioned() {
        let dir = Path::new("/tmp/imgcache");
        let a = cache_path(dir, "https://mouser.com/a.png");
        let b = cache_path(dir, "https://mouser.com/a.png");
        assert_eq!(a, b);
        assert!(a.extension().is_some_and(|e| e == "png"));
        // Different URLs → different files.
        assert_ne!(a, cache_path(dir, "https://mouser.com/b.png"));
    }

    #[test]
    fn image_signature_sniffing_rejects_html() {
        assert!(is_probably_image(&[0xFF, 0xD8, 0xFF, 0x00])); // JPEG
        assert!(is_probably_image(b"\x89PNG\r\n\x1a\nxxxx")); // PNG
        assert!(is_probably_image(b"GIF89a....")); // GIF
        assert!(is_probably_image(b"RIFF____WEBPVP8 ")); // WebP
                                                         // A bot-block / error page must NOT be treated as an image.
        assert!(!is_probably_image(b"<!DOCTYPE html><html>Access denied"));
        assert!(!is_probably_image(b""));
    }

    #[test]
    fn embed_source_reads_local_file_uris() {
        let dir = std::env::temp_dir().join(format!("lob-embed-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("part.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nDATA").unwrap();
        let src = format!("file://{}", png.display());
        assert_eq!(
            embed_source(&src, &dir),
            Some(to_data_uri(b"\x89PNG\r\n\x1a\nDATA", "image/png"))
        );
        // A missing file → None (degrade, don't panic).
        assert_eq!(embed_source("file:///no/such/file.jpg", &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cached_image_is_reused_without_network() {
        let dir = std::env::temp_dir().join(format!("lob-img-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let url = "https://example.invalid/part.jpg"; // unreachable — must not be hit
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0x78]; // valid JPEG signature
        std::fs::write(cache_path(&dir, url), jpeg).unwrap();
        assert_eq!(
            fetch_data_uri(url, &dir),
            Some(to_data_uri(&jpeg, "image/jpeg"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
