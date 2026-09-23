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

/// A crop rectangle over a source image, as **fractions** of its width and
/// height (`0.0..=1.0`).
///
/// Fractions rather than pixels so a crop survives the source being re-fetched
/// at a different resolution, and so the same rectangle means the same thing
/// whatever the shop serves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Crop {
    /// Whether this rectangle is usable: inside the image, and not degenerate.
    /// A bad crop is ignored rather than applied, because a photo must never be
    /// able to break the BOM.
    pub fn is_sane(&self) -> bool {
        self.w > 0.001
            && self.h > 0.001
            && self.x >= 0.0
            && self.y >= 0.0
            && self.x + self.w <= 1.0001
            && self.y + self.h <= 1.0001
    }
}

/// Where a source's crop is recorded: beside its cached image, keyed the same
/// way. Cropping a Thonk photo once therefore applies in every project that
/// references it — the same reasoning that makes the image cache shared.
fn crop_path(cache_dir: &Path, source: &str) -> PathBuf {
    cache_path(cache_dir, source).with_extension("crop")
}

/// The crop recorded for `source`, if any.
pub fn read_crop(cache_dir: &Path, source: &str) -> Option<Crop> {
    let text = std::fs::read_to_string(crop_path(cache_dir, source)).ok()?;
    let n: Vec<f64> = text
        .trim()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let [x, y, w, h] = n[..] else { return None };
    let crop = Crop { x, y, w, h };
    crop.is_sane().then_some(crop)
}

/// Record (or, with `None`, clear) the crop for `source`.
pub fn write_crop(cache_dir: &Path, source: &str, crop: Option<Crop>) -> std::io::Result<()> {
    let path = crop_path(cache_dir, source);
    match crop.filter(Crop::is_sane) {
        Some(c) => {
            std::fs::create_dir_all(cache_dir)?;
            std::fs::write(path, format!("{},{},{},{}", c.x, c.y, c.w, c.h))
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        },
    }
}

/// The bytes for an image *source* — `file://` local path or http(s) URL —
/// **uncropped**, fetching and caching a URL as needed.
///
/// This is what a crop editor needs: you cannot choose a rectangle over an image
/// you are only shown the inside of.
pub fn source_bytes(source: &str, cache_dir: &Path) -> Option<Vec<u8>> {
    if let Some(bytes) = cached_source_bytes(source, cache_dir) {
        return Some(bytes);
    }
    if source.starts_with("file://") {
        return None; // a local file that is missing or not an image
    }
    let bytes = download(source)?;
    let path = cache_path(cache_dir, source);
    let _ = std::fs::create_dir_all(cache_dir);
    let _ = std::fs::write(&path, &bytes);
    Some(bytes)
}

/// The bytes already on disk for a source, **never** going to the network.
///
/// What an HTTP endpoint should serve: fetching whatever URL a request names
/// would make the dashboard a proxy for arbitrary outbound GETs. Cropping only
/// ever touches photos the BOM has already resolved and cached, so cache-only is
/// no restriction in practice.
pub fn cached_source_bytes(source: &str, cache_dir: &Path) -> Option<Vec<u8>> {
    let path = match source.strip_prefix("file://") {
        Some(local) => PathBuf::from(local),
        None => cache_path(cache_dir, source),
    };
    // A cached file is trusted only if it still looks like an image (guards against
    // a stale cache written before payload validation existed).
    let bytes = std::fs::read(path).ok()?;
    is_probably_image(&bytes).then_some(bytes)
}

/// The MIME to serve a source's bytes as, from its extension.
pub fn source_mime(source: &str) -> &'static str {
    mime_from_url(source.strip_prefix("file://").unwrap_or(source))
}

/// The bytes for a source with its recorded crop applied, and the MIME to serve
/// them as. Falls back to the full image if the crop cannot be applied.
pub fn cropped_bytes(source: &str, cache_dir: &Path) -> Option<(Vec<u8>, &'static str)> {
    let bytes = source_bytes(source, cache_dir)?;
    let mime = source_mime(source);
    match read_crop(cache_dir, source) {
        Some(crop) => Some((apply_crop(&bytes, crop).unwrap_or(bytes), mime)),
        None => Some((bytes, mime)),
    }
}

/// Crop `bytes` to `crop` with macOS `sips`, as [`crate::fab::png_to_jpeg`] does
/// for format conversion. `None` when the tool is absent or the crop fails, and
/// the caller keeps the full image — a missing cropper degrades the picture, it
/// does not break the build.
fn apply_crop(bytes: &[u8], crop: Crop) -> Option<Vec<u8>> {
    use std::process::Command;
    if !crop.is_sane() {
        return None;
    }
    let dir = std::env::temp_dir();
    let stamp: String = Sha256::digest(bytes)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    let src = dir.join(format!("lob-crop-{stamp}-in"));
    let out = dir.join(format!("lob-crop-{stamp}-out"));
    std::fs::write(&src, bytes).ok()?;

    let dims = Command::new("sips")
        .args(["-g", "pixelWidth", "-g", "pixelHeight"])
        .arg(&src)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&dims.stdout);
    let num = |key: &str| -> Option<f64> {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key)?.trim().parse().ok())
    };
    let (w, h) = (num("pixelWidth:")?, num("pixelHeight:")?);

    // sips takes the crop as height/width plus a top/left offset, in pixels.
    let px = |v: f64| v.round().max(1.0) as i64;
    let ok = Command::new("sips")
        .args([
            "-c",
            &px(crop.h * h).to_string(),
            &px(crop.w * w).to_string(),
        ])
        .arg("--cropOffset")
        .args([
            (crop.y * h).round().max(0.0).to_string(),
            (crop.x * w).round().max(0.0).to_string(),
        ])
        .arg(&src)
        .arg("--out")
        .arg(&out)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let result = ok
        .then(|| std::fs::read(&out).ok())
        .flatten()
        .filter(|b| is_probably_image(b));
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
    result
}

/// Fetch an image by URL and return it as an embeddable `data:` URI, caching the
/// bytes under `cache_dir` and applying any crop recorded for it. A cached file
/// is reused without any network call (offline-friendly). Returns `None` on any
/// failure — callers degrade, never fail.
pub fn fetch_data_uri(url: &str, cache_dir: &Path) -> Option<String> {
    let (bytes, mime) = cropped_bytes(url, cache_dir)?;
    Some(to_data_uri(&bytes, mime))
}

/// Embed an image *source* — either a `file://` local path (a hand-attached
/// photo) or an http(s) URL (a distributor/curated image) — as a `data:` URI,
/// cropped to whatever rectangle was chosen for it. `None` on any failure, so
/// callers degrade to a swatch/blank.
pub fn embed_source(source: &str, cache_dir: &Path) -> Option<String> {
    let (bytes, mime) = cropped_bytes(source, cache_dir)?;
    Some(to_data_uri(&bytes, mime))
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

    /// A crop is recorded beside the cached image, survives a round trip, and
    /// clears cleanly. Keyed by source, so cropping a Thonk photo once applies
    /// everywhere that photo is used.
    #[test]
    fn a_crop_round_trips_and_clears() {
        let dir = std::env::temp_dir().join(format!("lob-crop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let url = "https://example.invalid/bag-of-jacks.jpg";

        assert_eq!(read_crop(&dir, url), None, "no crop to begin with");
        let crop = Crop {
            x: 0.25,
            y: 0.1,
            w: 0.5,
            h: 0.5,
        };
        write_crop(&dir, url, Some(crop)).unwrap();
        assert_eq!(read_crop(&dir, url), Some(crop));

        // Clearing is idempotent — clearing a crop that is already gone is fine.
        write_crop(&dir, url, None).unwrap();
        assert_eq!(read_crop(&dir, url), None);
        write_crop(&dir, url, None).unwrap();

        // A rectangle that runs off the image is refused rather than stored: a
        // bad crop must never be able to break a photo.
        let off = Crop {
            x: 0.8,
            y: 0.0,
            w: 0.5,
            h: 0.5,
        };
        assert!(!off.is_sane());
        write_crop(&dir, url, Some(off)).unwrap();
        assert_eq!(read_crop(&dir, url), None);
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
