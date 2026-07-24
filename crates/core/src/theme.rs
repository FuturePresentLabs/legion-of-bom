//! Shared visual identity for the generated documents — the build guide and the
//! Visual BOM / component sorting sheet. 5uj.8.
//!
//! A "bench technical manual" system: cool bench-paper, charcoal ink, a copper
//! component-lead accent, monospace for every piece of technical data, and an
//! engineering mm-grid substrate that echoes the grid a builder lays real parts
//! on. Deliberately self-contained and offline — system font stacks only, no
//! webfonts — so a guide or sheet looks identical opened straight off disk.
//!
//! Both documents include [`BASE_CSS`] then add their own rules, and open with a
//! shared [`masthead`]. The brand line is left generic on purpose (a parametric
//! brand identity is DESIGN 7.9); this is the typographic system it slots into.

/// Shared design tokens, base typography, the masthead / eyebrow / chip
/// components, the `.mono` and `.grid-cell` utilities, and the print base. A
/// document concatenates its own CSS after this.
pub const BASE_CSS: &str = "\
:root{--ink:#1c2024;--paper:#f1f3f1;--panel:#ffffff;--copper:#b0692f;\
--copper-soft:#efe6da;--solder:#2f7d57;--flux:#b5830c;--muted:#6c6a63;\
--line:#e2ddd2;--grid:#d8dedb}\
*{box-sizing:border-box}\
body{margin:0;background:var(--paper);color:var(--ink);\
font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;line-height:1.55;\
-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}\
.wrap{max-width:920px;margin:0 auto;padding:2.4rem 1.25rem}\
.mono{font-family:ui-monospace,'SF Mono','Cascadia Code',Menlo,Consolas,monospace;\
font-variant-numeric:tabular-nums}\
.masthead{border-bottom:1.5px solid var(--copper);padding-bottom:.9rem;margin-bottom:1.7rem}\
.eyebrow{font-family:ui-monospace,'SF Mono',Menlo,monospace;font-size:.7rem;letter-spacing:.24em;\
text-transform:uppercase;color:var(--copper);font-weight:600;margin:0 0 .4rem}\
.doc-title{font-size:2rem;font-weight:750;letter-spacing:-.015em;line-height:1.05;margin:0;\
text-wrap:balance}\
.doc-sub{color:var(--muted);margin:.55rem 0 0;max-width:64ch}\
.meta{display:flex;flex-wrap:wrap;gap:.4rem;margin-top:.85rem}\
.chip{font-family:ui-monospace,'SF Mono',Menlo,monospace;font-size:.72rem;letter-spacing:.02em;\
background:var(--copper-soft);color:#8a4f22;border-radius:999px;padding:.14rem .62rem;white-space:nowrap}\
.grid-cell{background-color:var(--panel);\
background-image:linear-gradient(var(--grid) .5px,transparent .5px),\
linear-gradient(90deg,var(--grid) .5px,transparent .5px);\
background-size:2mm 2mm;background-position:center;\
-webkit-print-color-adjust:exact;print-color-adjust:exact}\
@media print{@page{margin:13mm}body{background:#fff}.wrap{max-width:none;padding:0}}";

/// Render the shared document masthead: a doc-type eyebrow, the circuit-name
/// title, a one-line summary, and optional monospace metadata chips (kit type,
/// counts). `meta` chips are shown verbatim in a copper-tinted pill row.
pub fn masthead(eyebrow: &str, title: &str, sub: &str, meta: &[String]) -> String {
    let chips: String = meta
        .iter()
        .map(|m| format!("<span class=\"chip\">{}</span>", esc(m)))
        .collect();
    let meta_html = if meta.is_empty() {
        String::new()
    } else {
        format!("<div class=\"meta\">{chips}</div>")
    };
    format!(
        "<header class=\"masthead\"><p class=\"eyebrow\">{}</p>\
         <h1 class=\"doc-title\">{}</h1><p class=\"doc-sub\">{}</p>{meta_html}</header>",
        esc(eyebrow),
        esc(title),
        esc(sub),
    )
}

/// Minimal HTML/attribute escaping for text and attribute values, shared by every
/// document renderer.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masthead_has_eyebrow_title_and_chips() {
        let h = masthead(
            "Build guide",
            "Slew <Limiter>",
            "5 steps",
            &["Through-hole kit".into()],
        );
        assert!(h.contains("class=\"eyebrow\">Build guide<"));
        assert!(h.contains("Slew &lt;Limiter&gt;")); // escaped title
        assert!(h.contains("class=\"chip\">Through-hole kit<"));
    }

    #[test]
    fn masthead_omits_empty_meta_row() {
        assert!(!masthead("X", "Y", "Z", &[]).contains("class=\"meta\""));
    }
}
