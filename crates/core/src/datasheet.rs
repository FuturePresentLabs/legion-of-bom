//! Cited datasheet facts, checked against the PDF they cite (okm.11).
//!
//! A curated circuit family needs facts no CAD library carries — "two 2.2 µF
//! capacitors on VCAP", "0.1 µF on NRST". Those used to be exactly the facts
//! typed from memory. Here each one is a [`Citation`]: a **pinned** datasheet
//! (URL plus the SHA-256 of the bytes that were read), a page, and a verbatim
//! quote. [`verify`] fetches the pinned PDF, extracts its text page by page
//! with `pdftotext -layout`, and fails loudly unless the quote is on that page —
//! so a fact that was misremembered, misattributed or paraphrased cannot pass,
//! and a datasheet silently revised upstream is caught by its hash.
//!
//! Deliberately thin: URL + hash + page is the citation shape Research-Wing
//! could own later (legion-of-bom-up4), with this module becoming its client.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::stage::StageError;

/// One datasheet, pinned to the exact bytes its citations were read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datasheet {
    /// The part it documents (an MPN).
    pub part: &'static str,
    /// Where it came from: a distributor's own copy, never a web search.
    pub url: &'static str,
    /// SHA-256 of the PDF bytes, lowercase hex.
    pub sha256: &'static str,
}

/// A fact's source: a page of a pinned datasheet and the words on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Citation {
    pub source: &'static Datasheet,
    /// 1-based page, as `pdftotext` counts them (not the printed page label).
    pub page: usize,
    /// Verbatim text on that page. Matched after [`normalize`], so line breaks
    /// and µ-versus-μ do not matter; words do.
    pub quote: &'static str,
}

/// What a design fact rests on.
///
/// Some facts cannot be checked by machine: WM8731's supply decoupling exists
/// only in a schematic figure (p.60, no text layer) and OCR misreads it
/// (0.1uF comes back as "0.4uF"). Those are a [`Evidence::Reading`] — what a
/// reader saw on that page — and count only once a person has confirmed it,
/// the same human-verification gate the parts library applies to pinouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// Verbatim text on a page: checked mechanically by [`verify`].
    Quote(Citation),
    /// A value read off a figure, or a fact the pinned sources do not state.
    /// `source`/`page` say where to look; `None` means no pinned document
    /// states it at all.
    Reading {
        source: Option<&'static Datasheet>,
        page: usize,
        what: &'static str,
        /// Who confirmed the reading against the source (a person, by name).
        confirmed_by: Option<&'static str>,
    },
}

impl Evidence {
    /// The mechanically checkable citation, if this is one.
    pub fn quote(&self) -> Option<&Citation> {
        match self {
            Evidence::Quote(c) => Some(c),
            Evidence::Reading { .. } => None,
        }
    }

    /// A reading no person has confirmed yet — what the human gate reports.
    pub fn unconfirmed(&self) -> Option<String> {
        match self {
            Evidence::Reading {
                source,
                page,
                what,
                confirmed_by: None,
            } => Some(match source {
                Some(ds) => format!("{} p.{page}: {what}", ds.part),
                None => format!("unsourced: {what}"),
            }),
            _ => None,
        }
    }
}

/// The shared datasheet store (override with `LOB_DATASHEET_CACHE`), keyed by
/// hash so a revised upstream PDF can never overwrite a pinned one. Durable
/// data, not a cache: these are the evidence every catalog fact cites, kept
/// for Research-Wing to ingest, and a distributor's link can die.
pub fn default_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOB_DATASHEET_CACHE") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("legion-of-bom").join("datasheets")
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The pinned PDF on disk: from the cache when present and intact, else
/// downloaded and checked against its hash before it is kept.
pub fn fetch(ds: &Datasheet, cache_dir: &Path) -> Result<PathBuf, StageError> {
    fetch_pinned(ds.part, ds.url, ds.sha256, cache_dir)
}

/// [`fetch`] for a datasheet named by its parts rather than a [`Datasheet`].
pub fn fetch_pinned(
    part: &str,
    url: &str,
    sha256: &str,
    cache_dir: &Path,
) -> Result<PathBuf, StageError> {
    let path = cache_dir.join(format!("{sha256}.pdf"));
    if let Ok(bytes) = std::fs::read(&path) {
        if sha256_hex(&bytes) == sha256 {
            return Ok(path);
        }
    }
    let resp = ureq::get(url)
        .set("User-Agent", "Mozilla/5.0 (legion-of-bom datasheet fetch)")
        .call()
        .map_err(|e| StageError::Other(format!("fetching {part} datasheet {url}: {e}")))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut bytes)?;
    let got = sha256_hex(&bytes);
    if got != sha256 {
        return Err(StageError::Other(format!(
            "{} datasheet at {} is not the pinned revision (sha256 {got}, pinned {}): \
             re-read the citations against the new one before re-pinning",
            part, url, sha256
        )));
    }
    std::fs::create_dir_all(cache_dir)?;
    std::fs::write(&path, &bytes)?;
    Ok(path)
}

/// A PDF's text, one string per page (`pdftotext -layout`, split on form feeds).
pub fn pages(pdf: &Path) -> Result<Vec<String>, StageError> {
    let out = Command::new("pdftotext")
        .arg("-layout")
        .arg(pdf)
        .arg("-")
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StageError::ToolNotFound("pdftotext (poppler)".into()),
            _ => StageError::Io(e),
        })?;
    if !out.status.success() {
        return Err(StageError::ToolFailed {
            tool: "pdftotext".into(),
            code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pages: Vec<String> = text.split('\u{c}').map(str::to_string).collect();
    if pages.last().is_some_and(|p| p.trim().is_empty()) {
        pages.pop();
    }
    Ok(pages)
}

/// Text as a quote is compared: whitespace runs collapsed, and the characters
/// datasheets spell several ways (µ/μ, Ω/Ω, dashes, non-breaking spaces)
/// folded to one form. Case is kept: "VCAP" and "vcap" are not the same claim.
pub fn normalize(s: &str) -> String {
    let folded: String = s
        .chars()
        .map(|c| match c {
            '\u{3bc}' => '\u{b5}', // Greek mu -> micro sign
            // Symbol-font mu: PDFs set in Adobe Symbol carry its "m" glyph as the
            // private-use U+F06D, invisible in a terminal ("C2 = 1F" is 1 µF).
            '\u{f06d}' => '\u{b5}',
            '\u{2126}' => '\u{3a9}',                     // ohm sign -> Omega
            '\u{2013}' | '\u{2014}' | '\u{2212}' => '-', // dashes, minus
            '\u{a0}' | '\u{2009}' | '\u{202f}' => ' ',   // nbsp, thin spaces
            other => other,
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Is `c`'s quote on its page of `pages`? An error names the citation, and —
/// when the quote is elsewhere in the document — where it actually is.
pub fn check(c: &Citation, pages: &[String]) -> Result<(), String> {
    check_quote(c.source.part, c.page, c.quote, pages)
}

/// [`check`] for a quote named by its parts rather than a [`Citation`].
pub fn check_quote(part: &str, page: usize, quote: &str, pages: &[String]) -> Result<(), String> {
    let want = normalize(quote);
    let on = |i: usize| pages.get(i).is_some_and(|p| normalize(p).contains(&want));
    if page >= 1 && on(page - 1) {
        return Ok(());
    }
    let elsewhere: Vec<usize> = (0..pages.len()).filter(|&i| on(i)).map(|i| i + 1).collect();
    Err(format!(
        "{} p.{}: {:?} is not on that page{}",
        part,
        page,
        quote,
        if elsewhere.is_empty() {
            " or anywhere in the document".to_string()
        } else {
            format!(" (found on p.{elsewhere:?})")
        }
    ))
}

/// Check every citation against its pinned datasheet, fetching as needed.
/// Returns one line per failure; empty means every fact is on its page.
pub fn verify(citations: &[Citation], cache_dir: &Path) -> Result<Vec<String>, StageError> {
    let mut failures = Vec::new();
    let mut read: Vec<(&str, Vec<String>)> = Vec::new();
    for c in citations {
        let idx = match read.iter().position(|(h, _)| *h == c.source.sha256) {
            Some(i) => i,
            None => {
                let pdf = fetch(c.source, cache_dir)?;
                read.push((c.source.sha256, pages(&pdf)?));
                read.len() - 1
            }
        };
        if let Err(e) = check(c, &read[idx].1) {
            failures.push(e);
        }
    }
    Ok(failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DS: Datasheet = Datasheet {
        part: "TEST",
        url: "https://example.com/t.pdf",
        sha256: "00",
    };

    fn pages_of(text: &[&str]) -> Vec<String> {
        text.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_quote_matches_across_line_breaks_and_mu_spellings() {
        let pages = pages_of(&[
            "cover",
            "  Two external\n   capacitors of 2.2 \u{3bc}F   on VCAP",
        ]);
        let c = Citation {
            source: &DS,
            page: 2,
            quote: "Two external capacitors of 2.2 \u{b5}F on VCAP",
        };
        assert_eq!(check(&c, &pages), Ok(()));
    }

    #[test]
    fn a_misremembered_or_misplaced_fact_fails_and_says_where_it_really_is() {
        let pages = pages_of(&["VCAP 2.2 \u{b5}F", "nothing here"]);
        let wrong_page = Citation {
            source: &DS,
            page: 2,
            quote: "VCAP 2.2 \u{b5}F",
        };
        let err = check(&wrong_page, &pages).unwrap_err();
        assert!(err.contains("found on p.[1]"), "{err}");
        let invented = Citation {
            source: &DS,
            page: 1,
            quote: "VCAP 4.7 \u{b5}F",
        };
        let err = check(&invented, &pages).unwrap_err();
        assert!(err.contains("anywhere in the document"), "{err}");
    }

    /// WM8731 p.24 really reads "C2 = 1\u{f06d}F": the Symbol font's µ as a
    /// private-use character a terminal shows as nothing at all.
    #[test]
    fn a_symbol_font_mu_is_a_mu() {
        let pages = pages_of(&["C2 = 1\u{f06d}F, R1 = 680 \u{2126}"]);
        let c = Citation {
            source: &DS,
            page: 1,
            quote: "C2 = 1\u{b5}F, R1 = 680 \u{3a9}",
        };
        assert_eq!(check(&c, &pages), Ok(()));
    }

    #[test]
    fn page_zero_is_not_a_page() {
        let pages = pages_of(&["VCAP"]);
        let c = Citation {
            source: &DS,
            page: 0,
            quote: "VCAP",
        };
        assert!(check(&c, &pages).is_err());
    }
}
