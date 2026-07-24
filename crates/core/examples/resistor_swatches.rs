//! Render a resistor color-code reference card (5uj.1) — the actual Rust-generated
//! band pictograms, grouped by decade, so the palette and pictogram sizing can be
//! reviewed without a full guide run. Doubles as a printable bench sorting card.
//!
//! `cargo run -p legion-of-bom-core --example resistor_swatches > card.html`

use legion_of_bom_core::resistor::{color_code, digit_colors};

/// Value groups, each a section on the card.
const GROUPS: &[(&str, &[&str])] = &[
    ("Ohms", &["10", "22", "47", "100", "220", "470", "680"]),
    (
        "Kilohms",
        &[
            "1k", "2.2k", "4.7k", "10k", "22k", "47k", "100k", "220k", "470k",
        ],
    ),
    ("Megohms", &["1M", "2.2M"]),
    ("Sub-ohm & 1% (E96)", &["R47", "4R7", "49R9", "4k99"]),
];

fn main() {
    let mut body = String::new();

    // Digit→color legend, straight from the band palette (single source of truth).
    let mut legend = String::new();
    for (d, band) in digit_colors().iter().enumerate() {
        legend.push_str(&format!(
            "<div class=\"key\"><span class=\"chip\" style=\"background:{}\"></span>\
             <span class=\"kd\">{d}</span><span class=\"kn\">{}</span></div>",
            band.hex, band.name,
        ));
    }
    body.push_str(&format!(
        "<div class=\"legend\"><span class=\"eyebrow\">Digit → band color</span>\
         <div class=\"keys\">{legend}</div></div>"
    ));

    for (title, values) in GROUPS {
        let mut rows = String::new();
        for v in *values {
            let Some(cc) = color_code(v) else { continue };
            rows.push_str(&format!(
                "<tr><td class=\"pic\">{}</td><td class=\"val\">{v}</td>\
                 <td class=\"dec\">{}</td><td class=\"code\">{}-band</td></tr>",
                cc.to_svg(104.0, 30.0),
                cc.band_names(),
                if cc.five_band { 5 } else { 4 },
            ));
        }
        body.push_str(&format!(
            "<section><h2>{title}</h2><table>{rows}</table></section>"
        ));
    }

    print!(
        "<title>Resistor color-code reference</title>\
         <style>{STYLE}</style>\
         <header><span class=\"eyebrow\">Legion of BOM · Visual BOM</span>\
         <h1>Resistor color-code reference</h1>\
         <p class=\"lede\">Every value below is decoded to the bands printed on a through-hole \
         axial resistor, rendered by the same code that annotates the build-guide sort sheet — \
         so a builder can match a resistor to its value by eye. Tolerance defaults to gold (5%, \
         carbon film) on 4-band codes and brown (1%, metal film) on 5-band codes.</p></header>\
         <main>{body}</main>"
    );
}

const STYLE: &str = "\
:root{--paper:#faf9f6;--ink:#20242c;--muted:#6b6f76;--line:#e6e3db;--panel:#f2efe8;--copper:#b0692f}\
*{box-sizing:border-box}\
body{background:var(--paper);color:var(--ink);\
font-family:system-ui,-apple-system,Segoe UI,sans-serif;line-height:1.5;\
margin:0;padding:2.5rem 1.25rem;-webkit-font-smoothing:antialiased}\
header,main{max-width:760px;margin:0 auto}\
.eyebrow{display:inline-block;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;\
font-size:.7rem;letter-spacing:.14em;text-transform:uppercase;color:var(--copper);margin-bottom:.5rem}\
h1{font-size:1.65rem;font-weight:700;letter-spacing:-.01em;margin:.1rem 0 .6rem;text-wrap:balance}\
.lede{color:var(--muted);max-width:62ch;margin:0 0 1.5rem}\
.legend{border:1px solid var(--line);border-radius:10px;background:#fff;padding:1rem 1.1rem;margin-bottom:2rem}\
.keys{display:flex;flex-wrap:wrap;gap:.35rem .9rem;margin-top:.4rem}\
.key{display:flex;align-items:center;gap:.4rem;font-size:.82rem}\
.chip{width:14px;height:14px;border-radius:3px;border:1px solid #0002;flex:none}\
.kd{font-family:ui-monospace,Menlo,monospace;font-weight:600;color:var(--ink);width:.7em}\
.kn{color:var(--muted)}\
section{margin:0 0 1.75rem}\
h2{font-size:.82rem;font-weight:600;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);\
margin:0 0 .3rem;padding-bottom:.35rem;border-bottom:2px solid var(--copper);display:inline-block}\
table{width:100%;border-collapse:collapse;margin-top:.2rem}\
td{padding:.5rem .6rem;border-bottom:1px solid var(--line);vertical-align:middle}\
tr:last-child td{border-bottom:0}\
.pic{width:112px}.pic svg{display:block}\
.val{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-weight:600;font-size:1rem;\
font-variant-numeric:tabular-nums;width:5rem}\
.dec{color:var(--muted);font-size:.86rem}\
.code{text-align:right;font-family:ui-monospace,Menlo,monospace;font-size:.72rem;color:var(--muted);\
white-space:nowrap}\
@media(max-width:520px){.code{display:none}.lede{font-size:.95rem}}";
