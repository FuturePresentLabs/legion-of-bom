//! Shared visual identity for the generated documents — the build guide and the
//! Visual BOM / component sorting sheet. 5uj.8.
//!
//! **A photocopied kit manual, not a styled document.** The reference is the
//! sheet that comes folded in a Thonk or Befaco bag: black ink on white paper,
//! ruled tables, and pictures doing the work. It is not decoration-led, because
//! the reader is holding a soldering iron.
//!
//! Three rules keep it honest:
//!
//! 1. **Ink is black.** Text is `#000` on `#fff`. The previous system tinted the
//!    paper, greyed the body copy and ran a copper accent through every heading,
//!    rule and chip — which reads as a template applied to the content rather
//!    than as the content. It also photocopies and prints badly, which is the
//!    actual delivery medium.
//! 2. **One accent, and it means something.** Red is reserved for what can
//!    destroy the module: polarity, orientation, the −12 V end. If red appears,
//!    it is a thing you can get wrong. Nothing decorative may use it.
//! 3. **Structure comes from rules and weight**, not from pills, tints and
//!    rounded corners. A table looks like a table.
//!
//! Self-contained and offline — system font stacks only, no webfonts — so a
//! guide looks the same opened off a USB stick on somebody else's bench.
//!
//! Both documents include [`BASE_CSS`] then add their own rules, and open with a
//! shared [`masthead`]. The brand line is left generic on purpose (a parametric
//! brand identity is DESIGN 7.9); this is the typographic system it slots into.

/// Printable width in millimetres — the **intersection** of US Letter (216 mm)
/// and A4 (210 mm) at the [`PAGE_MARGIN_MM`] margin. Both documents lay out to
/// this, so one file prints true at 100% ("actual size", no shrink-to-fit) on
/// either paper. That matters beyond neatness: the sorting sheet's resistor
/// bands and package silhouettes are drawn life-size, and a scaled print is a
/// wrong ruler.
pub const CONTENT_W_MM: f64 = 186.0;
/// Printable height in millimetres — likewise the intersection of Letter
/// (279 mm) and A4 (297 mm), less margins.
pub const CONTENT_H_MM: f64 = 255.0;
/// Page margin both documents assume, and the one their `@page` rule sets.
pub const PAGE_MARGIN_MM: f64 = 12.0;

/// Shared design tokens, base typography, the masthead / eyebrow / chip
/// components, the `.mono` / `.chk` / `.grid-cell` utilities, and the print base.
/// A document concatenates its own CSS after this.
///
/// Print is the target, not an afterthought: the page box is fixed to
/// [`CONTENT_W_MM`] × [`CONTENT_H_MM`], colour is forced to print (the
/// highlight wash and the copper rules carry meaning, so a printer that drops
/// backgrounds would drop information), and the masthead compresses so a sheet
/// spends its area on content rather than on branding.
pub const BASE_CSS: &str = "\
:root{--ink:#000;--paper:#fff;--panel:#fff;--warn:#c8102e;--muted:#555;\
--line:#000;--hair:#b8b8b8;--grid:#dcdcdc}\
*{box-sizing:border-box}\
body{margin:0;background:var(--paper);color:var(--ink);\
font-family:'Helvetica Neue',Helvetica,Arial,system-ui,sans-serif;font-size:10pt;line-height:1.35;\
-webkit-print-color-adjust:exact;print-color-adjust:exact}\
.wrap{width:186mm;max-width:100%;margin:0 auto;padding:8mm 0}\
.mono{font-family:ui-monospace,'SF Mono',Menlo,Consolas,monospace;font-variant-numeric:tabular-nums}\
.masthead{border-bottom:1.6pt solid var(--ink);padding-bottom:1.5mm;margin-bottom:3mm;\
display:flex;flex-wrap:wrap;align-items:baseline;gap:.5mm 4mm}\
.eyebrow{font-size:8pt;font-weight:700;text-transform:uppercase;margin:0;order:-1;flex-basis:100%;\
letter-spacing:.02em}\
.doc-title{font-size:20pt;font-weight:700;letter-spacing:-.02em;line-height:1;margin:0}\
.doc-sub{margin:0;font-size:8.5pt;flex:1 1 34ch;min-width:0;line-height:1.3}\
.meta{display:flex;flex-wrap:wrap;gap:0;flex-basis:100%;margin-top:1mm;font-size:8pt;\
font-family:ui-monospace,'SF Mono',Menlo,monospace}\
.chip{white-space:nowrap}\
.chip+.chip::before{content:'  ·  ';color:var(--hair)}\
.chk{display:inline-block;width:3.6mm;height:3.6mm;border:.35mm solid var(--ink);\
background:#fff;vertical-align:-.7mm;flex:none}\
.docfoot{border-top:.5pt solid var(--hair);margin-top:auto;padding-top:1.2mm;\
font-size:7pt;color:var(--muted);display:flex;justify-content:space-between;gap:1rem}\
.grid-cell{background-color:var(--panel);\
background-image:linear-gradient(var(--grid) .4px,transparent .4px),\
linear-gradient(90deg,var(--grid) .4px,transparent .4px);\
background-size:5mm 5mm;background-position:center}\
@media print{@page{size:auto;margin:12mm}.wrap{width:auto;padding:0}}";

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

/// A per-sheet footer: what this page is, on the left, and where you are in the
/// document, on the right.
///
/// Printed guides get separated — a sheet on the bench, a sheet on the floor —
/// and a page with no identity is a page you can't put back. Chrome's `@page`
/// margin boxes are the "proper" mechanism and are not reliably implemented, so
/// each sheet carries its own footer in flow instead.
pub fn page_footer(left: &str, right: &str) -> String {
    format!(
        "<p class=\"docfoot\"><span>{}</span><span>{}</span></p>",
        esc(left),
        esc(right),
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
