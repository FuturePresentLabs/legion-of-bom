//! Build guide — a step-by-step visual assembly guide (DESIGN.md 7.6/7.8).
//!
//! Parses a generated `.kicad_pcb` for component positions (decoupled from board
//! generation, like [`drc`](crate::drc)/[`fab`](crate::fab)), groups the parts
//! into low-profile-first build steps, and renders a self-contained HTML page.
//! Each step shows a top-down board diagram with *that* step's parts highlighted
//! — the "red boxes over all the resistors, then the caps" a human follows —
//! plus a sorted parts list and polarity/pin-1 callouts.
//!
//! Values and part types come from the circuit; positions come from the board.
//! The diagram is a schematic top-down (accurate boxes, no render/camera
//! dependency); overlaying on a photoreal render is a later refinement.

use std::collections::{BTreeMap, HashSet};

use crate::pdf::{self, Font, Page, Paint};
use crate::sexpr::Sexpr;
use crate::source::CircuitSource;

/// A placed component: board-space centre + pad bounding box (mm).
#[derive(Debug, Clone)]
pub struct PlacedPart {
    pub refdes: String,
    pub value: String,
    pub footprint: String,
    pub cx: f64,
    pub cy: f64,
    pub bbox: (f64, f64, f64, f64),
    pub back: bool,
    /// Position of the reference pad (pin 1) — where the polarity marker sits.
    pub pin1: Option<(f64, f64)>,
    /// Polarity/orientation reference, for polarised parts only.
    pub polarity: Option<Polarity>,
}

/// A polarity/orientation reference: what to align to the board's silkscreen
/// mark, resolved per part (a ceramic cap has none, an electrolytic has `Plus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    /// Diode/LED cathode — the banded / flat end.
    Cathode,
    /// Positive terminal of a polarised (electrolytic/tantalum) capacitor.
    Plus,
    /// Pin 1 of an IC / connector / transistor (notch / dot / flat).
    Pin1,
}

impl Polarity {
    /// Short marker drawn at the reference pad on the diagram.
    fn label(self) -> &'static str {
        match self {
            Polarity::Cathode => "K",
            Polarity::Plus => "+",
            Polarity::Pin1 => "1",
        }
    }
    /// The assembly caution, phrased against the board silkscreen.
    fn caution(self) -> &'static str {
        match self {
            Polarity::Cathode => {
                "Polarity: match each diode/LED cathode (K — banded/flat end) to the silkscreen band."
            }
            Polarity::Plus => {
                "Polarity: match each capacitor's + terminal to the silkscreen + / stripe."
            }
            Polarity::Pin1 => "Orientation: align pin 1 (notch/dot) to the silkscreen pin-1 mark.",
        }
    }
}

/// Resolve a part's polarity from its reference designator and footprint. A
/// ceramic/film cap is unpolarised; an electrolytic/tantalum one is `Plus`.
fn detect_polarity(refdes: &str, footprint: &str) -> Option<Polarity> {
    let name = footprint
        .rsplit(':')
        .next()
        .unwrap_or(footprint)
        .to_ascii_uppercase();
    match prefix_of(refdes) {
        "D" => Some(Polarity::Cathode),
        "U" | "Q" | "J" => Some(Polarity::Pin1),
        "C" if name.starts_with("CP")
            || name.contains("ELEC")
            || name.contains("TANTAL")
            || name.contains("POLAR") =>
        {
            Some(Polarity::Plus)
        }
        _ => None,
    }
}

/// One build step: a group of same-kind parts placed together.
#[derive(Debug, Clone)]
pub struct BuildStep {
    pub title: String,
    pub parts: Vec<PlacedPart>,
    /// A polarity / orientation warning to show, if the parts are polarised.
    pub caution: Option<String>,
}

/// The whole guide: the board outline + the ordered steps.
#[derive(Debug, Clone)]
pub struct BuildGuide {
    pub name: String,
    pub outline: (f64, f64, f64, f64),
    pub steps: Vec<BuildStep>,
}

/// A build-step kind, in low-profile-first assembly order (DESIGN 7.8). Polarity
/// cautions are derived per part (see [`detect_polarity`]), not fixed per kind —
/// a ceramic-cap step shows no caution, an electrolytic one does.
struct Kind {
    prefix: &'static str,
    title: &'static str,
}

// Order matters — this *is* the build sequence.
const KINDS: &[Kind] = &[
    Kind {
        prefix: "R",
        title: "Resistors",
    },
    Kind {
        prefix: "D",
        title: "Diodes & LEDs",
    },
    Kind {
        prefix: "J",
        title: "Connectors, sockets & headers",
    },
    Kind {
        prefix: "SW",
        title: "Switches",
    },
    Kind {
        prefix: "C",
        title: "Capacitors",
    },
    Kind {
        prefix: "Q",
        title: "Transistors",
    },
    Kind {
        prefix: "U",
        title: "ICs",
    },
];

/// Build the guide from a circuit (values, types) and its generated board
/// (positions). Parts default to the front; back parts are noted per step.
pub fn build_guide(circuit: &dyn CircuitSource, board_pcb: &str) -> Result<BuildGuide, String> {
    let placed = parse_board(board_pcb)?;
    let values: BTreeMap<&str, &str> = circuit
        .parts()
        .iter()
        .map(|p| (p.refdes.0.as_str(), p.value.as_str()))
        .collect();

    // Attach the circuit value + resolve polarity per part.
    let mut parts: Vec<PlacedPart> = placed
        .into_iter()
        .map(|mut p| {
            if let Some(v) = values.get(p.refdes.as_str()) {
                p.value = v.to_string();
            }
            p.polarity = detect_polarity(&p.refdes, &p.footprint);
            p
        })
        .collect();
    parts.sort_by_key(|p| refdes_key(&p.refdes));

    // Group into ordered steps by refdes prefix; anything unmatched is a final
    // "remaining parts" step so nothing is silently dropped.
    let mut steps = Vec::new();
    let mut used = vec![false; parts.len()];
    for kind in KINDS {
        let idxs: Vec<usize> = parts
            .iter()
            .enumerate()
            .filter(|(i, p)| !used[*i] && prefix_of(&p.refdes) == kind.prefix)
            .map(|(i, _)| i)
            .collect();
        if idxs.is_empty() {
            continue;
        }
        let group: Vec<PlacedPart> = idxs
            .iter()
            .map(|&i| {
                used[i] = true;
                parts[i].clone()
            })
            .collect();
        let caution = step_caution(&group);
        steps.push(BuildStep {
            title: kind.title.to_string(),
            parts: group,
            caution,
        });
    }
    let remaining: Vec<PlacedPart> = parts
        .iter()
        .enumerate()
        .filter(|(i, _)| !used[*i])
        .map(|(_, p)| p.clone())
        .collect();
    if !remaining.is_empty() {
        let caution = step_caution(&remaining);
        steps.push(BuildStep {
            title: "Remaining parts".to_string(),
            parts: remaining,
            caution,
        });
    }

    Ok(BuildGuide {
        name: circuit.name().to_string(),
        outline: board_outline(board_pcb).unwrap_or((0.0, 0.0, 10.0, 10.0)),
        steps,
    })
}

/// A step's polarity caution — from its first polarised part (parts in a
/// kind-step share a polarity), or `None` if nothing in the step is polarised.
fn step_caution(parts: &[PlacedPart]) -> Option<String> {
    parts
        .iter()
        .find_map(|p| p.polarity)
        .map(|pol| pol.caution().to_string())
}

/// The leading letters of a refdes (`R12` → `R`, `SW1` → `SW`).
fn prefix_of(refdes: &str) -> &str {
    let end = refdes
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(refdes.len());
    &refdes[..end]
}

/// Sort key: prefix then numeric index (`R2` before `R10`).
fn refdes_key(refdes: &str) -> (String, u64) {
    let p = prefix_of(refdes);
    let n = refdes[p.len()..].parse().unwrap_or(u64::MAX);
    (p.to_string(), n)
}

/// Parse footprints from a `.kicad_pcb`: refdes, centre, pad bounding box, side.
fn parse_board(board_pcb: &str) -> Result<Vec<PlacedPart>, String> {
    let root = Sexpr::parse(board_pcb)?;
    let mut parts = Vec::new();
    for fp in root.get_all("footprint") {
        let at = fp.get("at");
        let (fx, fy) = (
            at.and_then(|a| a.nth_atom(1)).and_then(f).unwrap_or(0.0),
            at.and_then(|a| a.nth_atom(2)).and_then(f).unwrap_or(0.0),
        );
        let refdes = fp
            .get_all("property")
            .into_iter()
            .find(|p| p.nth_atom(1) == Some("Reference"))
            .and_then(|p| p.nth_atom(2))
            .unwrap_or_default()
            .to_string();
        if refdes.is_empty() {
            continue;
        }
        let back = fp.get("layer").and_then(|l| l.nth_atom(1)) == Some("B.Cu");
        let footprint = fp.nth_atom(1).unwrap_or_default().to_string();

        // Pad bounding box, in board coordinates (rotation-0 grid placement), and
        // the position of pad 1 (the polarity/pin-1 reference).
        let mut bb = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        let mut pin1 = None;
        for pad in fp.get_all("pad") {
            let pat = pad.get("at");
            let (px, py) = (
                pat.and_then(|a| a.nth_atom(1)).and_then(f).unwrap_or(0.0),
                pat.and_then(|a| a.nth_atom(2)).and_then(f).unwrap_or(0.0),
            );
            let size = pad.get("size");
            let (pw, ph) = (
                size.and_then(|s| s.nth_atom(1)).and_then(f).unwrap_or(0.5),
                size.and_then(|s| s.nth_atom(2)).and_then(f).unwrap_or(0.5),
            );
            let (x, y) = (fx + px, fy + py);
            bb.0 = bb.0.min(x - pw / 2.0);
            bb.1 = bb.1.min(y - ph / 2.0);
            bb.2 = bb.2.max(x + pw / 2.0);
            bb.3 = bb.3.max(y + ph / 2.0);
            if pad.nth_atom(1) == Some("1") {
                pin1 = Some((x, y));
            }
        }
        if !bb.0.is_finite() {
            bb = (fx - 0.5, fy - 0.5, fx + 0.5, fy + 0.5);
        }
        parts.push(PlacedPart {
            refdes,
            value: String::new(),
            footprint,
            cx: fx,
            cy: fy,
            bbox: bb,
            back,
            pin1,
            polarity: None,
        });
    }
    Ok(parts)
}

/// The board outline from the `Edge.Cuts` rectangle, if present.
fn board_outline(board_pcb: &str) -> Option<(f64, f64, f64, f64)> {
    let root = Sexpr::parse(board_pcb).ok()?;
    let rect = root
        .get_all("gr_rect")
        .into_iter()
        .find(|r| r.get("layer").and_then(|l| l.nth_atom(1)) == Some("Edge.Cuts"))?;
    let start = rect.get("start")?;
    let end = rect.get("end")?;
    Some((
        f(start.nth_atom(1)?)?,
        f(start.nth_atom(2)?)?,
        f(end.nth_atom(1)?)?,
        f(end.nth_atom(2)?)?,
    ))
}

fn f(s: &str) -> Option<f64> {
    s.parse().ok()
}

/// Render the guide as a self-contained HTML page (inline CSS + one board diagram
/// per step, the step's parts highlighted). When `board_svg` is `Some`, it is
/// KiCad's real board plot (from [`fab::export_board_svg`](crate::fab)) used as
/// the underlay with highlight boxes overlaid; otherwise a schematic top-down.
pub fn guide_to_html(guide: &BuildGuide, board_svg: Option<&str>) -> String {
    let total = guide.steps.len();
    let mut body = String::new();
    body.push_str(&format!(
        "<h1>Build guide — {}</h1><p class=\"sub\">{total} steps, low-profile first. \
         Highlighted parts are for the current step.</p>",
        esc(&guide.name)
    ));
    for (i, step) in guide.steps.iter().enumerate() {
        let highlight: HashSet<&str> = step.parts.iter().map(|p| p.refdes.as_str()).collect();
        let svg = match board_svg {
            Some(kicad) => real_board_svg(kicad, guide.outline, &step.parts),
            None => schematic_board_svg(guide, &highlight),
        };
        let mut list = String::new();
        for (value, refs) in group_by_value(&step.parts) {
            list.push_str(&format!(
                "<li><b>{}×</b> {} — <span class=\"refs\">{}</span></li>",
                refs.len(),
                esc(&value),
                esc(&refs.join(", "))
            ));
        }
        let caution = step
            .caution
            .as_deref()
            .map(|c| format!("<p class=\"caution\">⚠ {}</p>", esc(c)))
            .unwrap_or_default();
        let back = step.parts.iter().any(|p| p.back);
        let back_note = if back {
            "<p class=\"caution\">↺ Some parts on this step mount on the BACK of the board.</p>"
        } else {
            ""
        };
        body.push_str(&format!(
            "<section class=\"step\"><h2>Step {} of {total}: {}</h2>\
             <div class=\"cols\"><div class=\"diagram\">{svg}</div>\
             <div class=\"parts\"><ul>{list}</ul>{caution}{back_note}</div></div></section>",
            i + 1,
            esc(&step.title),
        ));
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Build guide — {}</title>\
         <style>{CSS}</style></head><body>{body}</body></html>",
        esc(&guide.name)
    )
}

const CSS: &str = "\
body{font-family:system-ui,sans-serif;max-width:1000px;margin:2rem auto;padding:0 1rem;color:#222}\
h1{margin-bottom:.2rem}.sub{color:#666;margin-top:0}\
.step{border-top:2px solid #eee;padding:1.2rem 0}h2{font-size:1.15rem}\
.cols{display:flex;gap:1.5rem;flex-wrap:wrap;align-items:flex-start}\
.diagram{flex:1 1 340px;min-width:280px}.parts{flex:1 1 240px}\
svg{width:100%;height:auto;border:1px solid #eee;background:#fafafa;border-radius:6px}\
ul{margin:0;padding-left:1.1rem}li{margin:.15rem 0}.refs{color:#555}\
.caution{background:#fff6e0;border-left:3px solid #e0a800;padding:.5rem .7rem;border-radius:4px;margin:.6rem 0 0}\
@media print{@page{margin:12mm;size:A4}body{margin:0;max-width:none}\
.step{break-before:page;break-inside:avoid;border-top:none;padding:0}\
.step:first-of-type{break-before:avoid}h1,.sub{break-after:avoid}\
.diagram,.parts{break-inside:avoid}svg{max-height:150mm}}";

/// Draw a highlight marker for one placed part on a PDF page: a red box (filled
/// over the schematic fallback; outlined over the real-board image so the part
/// shows through), its refdes, and any polarity marker. `mapx`/`mapy` map board
/// mm to page points.
fn pdf_marker(
    pg: &mut Page,
    p: &PlacedPart,
    mapx: &dyn Fn(f64) -> f64,
    mapy: &dyn Fn(f64) -> f64,
    scale: f64,
    filled: bool,
) {
    let (cx0, cy0, cx1, cy1) = p.bbox;
    pg.set_line_width(if filled { 0.8 } else { 1.2 });
    pg.set_stroke(0.63, 0.07, 0.07);
    if filled {
        pg.set_fill(0.89, 0.29, 0.29);
        pg.rect(
            mapx(cx0),
            mapy(cy1),
            ((cx1 - cx0) * scale).max(1.0),
            ((cy1 - cy0) * scale).max(1.0),
            Paint::FillStroke,
        );
    } else {
        pg.rect(
            mapx(cx0),
            mapy(cy1),
            ((cx1 - cx0) * scale).max(1.0),
            ((cy1 - cy0) * scale).max(1.0),
            Paint::Stroke,
        );
    }
    let fs = (scale * 1.4).clamp(5.0, 11.0);
    if filled {
        pg.set_fill(1.0, 1.0, 1.0);
    } else {
        pg.set_fill(0.63, 0.07, 0.07);
    }
    pg.text(
        mapx(p.cx) - p.refdes.len() as f64 * fs * 0.25,
        mapy(p.cy) - fs * 0.35,
        fs,
        Font::Bold,
        &p.refdes,
    );
    if let (Some(pol), Some((qx, qy))) = (p.polarity, p.pin1) {
        let r = (scale * 1.3).clamp(4.0, 9.0);
        pg.set_line_width(0.4);
        pg.set_fill(0.06, 0.06, 0.06);
        pg.set_stroke(1.0, 1.0, 1.0);
        pg.circle(mapx(qx), mapy(qy), r, Paint::FillStroke);
        pg.set_fill(1.0, 1.0, 1.0);
        pg.text(
            mapx(qx) - r * 0.3,
            mapy(qy) - r * 0.5,
            r * 1.2,
            Font::Bold,
            pol.label(),
        );
    }
}

/// Render the guide as a print-ready PDF — one build step per A4 page (clean page
/// breaks). Self-contained (no browser). When `board_jpeg` is `Some` (KiCad's
/// real board plot rasterized to JPEG, A4 landscape / true mm), it's embedded as
/// the diagram with highlight overlays; otherwise a schematic top-down.
pub fn guide_to_pdf(guide: &BuildGuide, board_jpeg: Option<&[u8]>) -> Vec<u8> {
    let image = board_jpeg.and_then(|b| pdf::Image::from_jpeg(b.to_vec()));
    let m = 36.0; // page margin (pt)
    let cw = pdf::A4_W - 2.0 * m; // content width
    let total = guide.steps.len();
    let (ox0, oy0, ox1, oy1) = guide.outline;
    let (pad, top) = (2.0, pdf::A4_H - m);
    let (bx0, by0, bx1, by1) = (ox0 - pad, oy0 - pad, ox1 + pad, oy1 + pad);
    let (bw, bh) = ((bx1 - bx0).max(1.0), (by1 - by0).max(1.0));

    let mut pages = Vec::new();
    for (i, step) in guide.steps.iter().enumerate() {
        let mut pg = Page::new();
        pg.set_fill(0.1, 0.1, 0.1);
        pg.text(
            m,
            top,
            13.0,
            Font::Regular,
            &format!("{} - Step {} of {total}", guide.name, i + 1),
        );
        pg.text(m, top - 24.0, 19.0, Font::Bold, &step.title);

        // Board diagram, scaled to fit the region under the title.
        let diag_top = top - 46.0; // page y of the board's top edge
        let scale = (cw / bw).min(360.0 / bh);
        let dw = bw * scale;
        let rx = m + (cw - dw) / 2.0;
        let mapx = |x: f64| rx + (x - bx0) * scale;
        let mapy = |y: f64| diag_top - (y - by0) * scale;
        let highlight: HashSet<&str> = step.parts.iter().map(|p| p.refdes.as_str()).collect();

        if image.is_some() {
            // Real KiCad board (A4-landscape JPEG in true mm) placed so board mm
            // map via mapx/mapy, clipped to the board region; then overlay this
            // step's parts as outlined markers so the real board shows through.
            let cm = [
                pdf::A4_H * scale / pdf::MM, // 297 mm × scale (pt/mm) in points
                0.0,
                0.0,
                pdf::A4_W * scale / pdf::MM, // 210 mm × scale
                mapx(0.0),
                mapy(pdf::A4_W / pdf::MM),
            ];
            let clip = (
                mapx(bx0),
                mapy(by1),
                (bx1 - bx0) * scale,
                (by1 - by0) * scale,
            );
            pg.draw_image(cm, clip);
            for p in &step.parts {
                pdf_marker(&mut pg, p, &mapx, &mapy, scale, false);
            }
        } else {
            pg.set_line_width(0.8);
            pg.set_fill(0.93, 0.95, 0.93);
            pg.set_stroke(0.2, 0.6, 0.4);
            pg.rect(
                mapx(ox0),
                mapy(oy1),
                (ox1 - ox0) * scale,
                (oy1 - oy0) * scale,
                Paint::FillStroke,
            );
            for p in guide.steps.iter().flat_map(|s| &s.parts) {
                let (cx0, cy0, cx1, cy1) = p.bbox;
                if highlight.contains(p.refdes.as_str()) {
                    pdf_marker(&mut pg, p, &mapx, &mapy, scale, true);
                } else {
                    pg.set_line_width(0.4);
                    pg.set_fill(0.86, 0.86, 0.86);
                    pg.set_stroke(0.67, 0.67, 0.67);
                    pg.rect(
                        mapx(cx0),
                        mapy(cy1),
                        ((cx1 - cx0) * scale).max(1.0),
                        ((cy1 - cy0) * scale).max(1.0),
                        Paint::FillStroke,
                    );
                }
            }
        }

        // Parts list + cautions below the diagram.
        let mut ly = (diag_top - bh * scale) - 30.0;
        pg.set_fill(0.13, 0.13, 0.13);
        pg.text(m, ly, 12.0, Font::Bold, "Parts for this step:");
        ly -= 18.0;
        for (value, refs) in group_by_value(&step.parts) {
            pg.set_fill(0.13, 0.13, 0.13);
            pg.text(
                m + 8.0,
                ly,
                11.0,
                Font::Regular,
                &format!("{}x   {}   ({})", refs.len(), value, refs.join(", ")),
            );
            ly -= 15.0;
        }
        if let Some(c) = &step.caution {
            ly -= 8.0;
            pg.set_fill(0.72, 0.45, 0.0);
            pg.text(m, ly, 11.0, Font::Bold, &format!("[!] {c}"));
            ly -= 15.0;
        }
        if step.parts.iter().any(|p| p.back) {
            pg.set_fill(0.72, 0.45, 0.0);
            pg.text(
                m,
                ly,
                11.0,
                Font::Bold,
                "[back] Some parts on this step mount on the BACK of the board.",
            );
        }
        pages.push(pg);
    }
    pdf::document(&pages, image.as_ref())
}

/// The highlight overlay for one part (SVG, board-mm coords): a red box + white
/// refdes + a polarity marker (K/+/1) at the reference pad, so the assembler can
/// match it to the board's silkscreen mark. `fill_opacity` lets the real board
/// show through when overlaid.
fn highlight_svg(p: &PlacedPart, fs: f64, fill_opacity: f64) -> String {
    let (bx0, by0, bx1, by1) = p.bbox;
    let mut s = format!(
        "<rect x=\"{bx0}\" y=\"{by0}\" width=\"{}\" height=\"{}\" rx=\"0.2\" fill=\"#e34a4a\" \
         fill-opacity=\"{fill_opacity}\" stroke=\"#a11\" stroke-width=\"0.35\"/>\
         <text x=\"{}\" y=\"{}\" font-size=\"{fs}\" text-anchor=\"middle\" \
         dominant-baseline=\"middle\" fill=\"#fff\" font-weight=\"bold\">{}</text>",
        bx1 - bx0,
        by1 - by0,
        p.cx,
        p.cy,
        esc(&p.refdes)
    );
    if let (Some(pol), Some((mx, my))) = (p.polarity, p.pin1) {
        let r = fs * 0.9;
        s.push_str(&format!(
            "<circle cx=\"{mx}\" cy=\"{my}\" r=\"{r}\" fill=\"#111\" stroke=\"#fff\" \
             stroke-width=\"0.15\"/>\
             <text x=\"{mx}\" y=\"{my}\" font-size=\"{}\" text-anchor=\"middle\" \
             dominant-baseline=\"central\" fill=\"#fff\" font-weight=\"bold\">{}</text>",
            fs * 1.1,
            pol.label(),
        ));
    }
    s
}

/// The board pad bounding box's short dimension → a legible label size (mm).
fn label_size(outline: (f64, f64, f64, f64)) -> f64 {
    let (x0, y0, x1, y1) = outline;
    ((x1 - x0).min(y1 - y0) / 30.0).clamp(0.7, 2.0)
}

/// A schematic top-down SVG: outline + every part as a box, `highlight`ed parts
/// red, the rest greyed. The fallback when no real KiCad plot is available.
fn schematic_board_svg(guide: &BuildGuide, highlight: &HashSet<&str>) -> String {
    let (x0, y0, x1, y1) = guide.outline;
    let (w, h) = (x1 - x0, y1 - y0);
    let pad = 2.0;
    let mut svg = format!(
        "<svg viewBox=\"{} {} {} {}\" xmlns=\"http://www.w3.org/2000/svg\">\
         <rect x=\"{x0}\" y=\"{y0}\" width=\"{w}\" height=\"{h}\" fill=\"#eef3ee\" \
         stroke=\"#3a6\" stroke-width=\"0.3\"/>",
        x0 - pad,
        y0 - pad,
        w + 2.0 * pad,
        h + 2.0 * pad
    );
    let fs = label_size(guide.outline);
    for p in guide.steps.iter().flat_map(|s| &s.parts) {
        if highlight.contains(p.refdes.as_str()) {
            svg.push_str(&highlight_svg(p, fs, 0.85));
        } else {
            let (bx0, by0, bx1, by1) = p.bbox;
            svg.push_str(&format!(
                "<rect x=\"{bx0}\" y=\"{by0}\" width=\"{}\" height=\"{}\" rx=\"0.2\" fill=\"#dcdcdc\" \
                 fill-opacity=\"0.5\" stroke=\"#aaa\" stroke-width=\"0.15\"/>",
                bx1 - bx0,
                by1 - by0
            ));
        }
    }
    svg.push_str("</svg>");
    svg
}

/// KiCad's real board plot with the current step's parts highlighted on top. The
/// KiCad SVG is in true board mm, so retargeting its viewBox to the board outline
/// crops/zooms to the board and the highlight overlays (also board-mm) align.
fn real_board_svg(kicad_svg: &str, outline: (f64, f64, f64, f64), parts: &[PlacedPart]) -> String {
    let (x0, y0, x1, y1) = outline;
    let pad = 2.0;
    let vb = format!(
        "{:.3} {:.3} {:.3} {:.3}",
        x0 - pad,
        y0 - pad,
        (x1 - x0) + 2.0 * pad,
        (y1 - y0) + 2.0 * pad
    );
    let base = retarget_viewbox(kicad_svg, &vb);
    let fs = label_size(outline);
    let overlay: String = parts.iter().map(|p| highlight_svg(p, fs, 0.42)).collect();
    base.replacen("</svg>", &format!("{overlay}</svg>"), 1)
}

/// Replace an SVG document's `viewBox` value (crop/zoom without moving content).
fn retarget_viewbox(svg: &str, viewbox: &str) -> String {
    let key = "viewBox=\"";
    if let Some(i) = svg.find(key) {
        let start = i + key.len();
        if let Some(len) = svg[start..].find('"') {
            return format!("{}{viewbox}{}", &svg[..start], &svg[start + len..]);
        }
    }
    svg.to_string()
}

/// Group a step's parts by value, preserving refdes order.
fn group_by_value(parts: &[PlacedPart]) -> Vec<(String, Vec<String>)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in parts {
        let v = if p.value.is_empty() {
            "(no value)".to_string()
        } else {
            p.value.clone()
        };
        if !map.contains_key(&v) {
            order.push(v.clone());
        }
        map.entry(v).or_default().push(p.refdes.clone());
    }
    order
        .into_iter()
        .map(|v| (v.clone(), map[&v].clone()))
        .collect()
}

/// Minimal HTML/XML escaping for text content and attributes.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};

    const BOARD: &str = r#"(kicad_pcb
      (gr_rect (start 95 95) (end 130 105) (layer "Edge.Cuts"))
      (footprint "R" (layer "F.Cu") (at 100 100 0)
        (property "Reference" "R1") (pad "1" smd rect (at -1 0) (size 1 1)) (pad "2" smd rect (at 1 0) (size 1 1)))
      (footprint "R" (layer "F.Cu") (at 110 100 0)
        (property "Reference" "R2") (pad "1" smd rect (at -1 0) (size 1 1)) (pad "2" smd rect (at 1 0) (size 1 1)))
      (footprint "U" (layer "F.Cu") (at 120 100 0)
        (property "Reference" "U1") (pad "1" smd rect (at -2 0) (size 1 1)) (pad "8" smd rect (at 2 0) (size 1 1))))"#;

    fn amp() -> Circuit {
        Circuit {
            name: "amp".into(),
            parts: vec![
                Part::new("R1", "9k"),
                Part::new("R2", "1k"),
                Part::new("U1", "TL072"),
            ],
            nets: vec![Net::new("N", vec![PinRef::new("R1", "1")])],
        }
    }

    #[test]
    fn orders_steps_low_profile_first_with_values() {
        let g = build_guide(&amp(), BOARD).unwrap();
        // Resistors before ICs.
        assert_eq!(g.steps.len(), 2);
        assert_eq!(g.steps[0].title, "Resistors");
        assert_eq!(g.steps[1].title, "ICs");
        assert!(g.steps[1].caution.as_deref().unwrap().contains("pin 1"));
        // Values attached from the circuit.
        assert_eq!(g.steps[0].parts.len(), 2);
        assert!(g.steps[0].parts.iter().any(|p| p.value == "9k"));
        assert_eq!(g.outline, (95.0, 95.0, 130.0, 105.0));
    }

    #[test]
    fn html_highlights_and_is_self_contained() {
        let g = build_guide(&amp(), BOARD).unwrap();
        let html = guide_to_html(&g, None);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<svg"));
        assert!(html.contains("Step 1 of 2: Resistors"));
        // The IC step cautions about pin 1.
        assert!(html.contains("pin 1"));
    }

    #[test]
    fn prefix_and_key() {
        assert_eq!(prefix_of("R12"), "R");
        assert_eq!(prefix_of("SW1"), "SW");
        assert!(refdes_key("R2") < refdes_key("R10"));
    }

    #[test]
    fn polarity_is_per_part_footprint_aware() {
        // Ceramic cap: not polarised. Electrolytic/tantalum: +.
        assert_eq!(
            detect_polarity("C1", "Capacitor_SMD:C_0805_2012Metric"),
            None
        );
        assert_eq!(
            detect_polarity("C2", "Capacitor_SMD:CP_Elec_5x5.4"),
            Some(Polarity::Plus)
        );
        assert_eq!(
            detect_polarity("C3", "Capacitor_THT:CP_Radial_Tantalum"),
            Some(Polarity::Plus)
        );
        // Diodes/LEDs → cathode; ICs/transistors/connectors → pin 1.
        assert_eq!(
            detect_polarity("D1", "Diode_SMD:D_SOD-123"),
            Some(Polarity::Cathode)
        );
        assert_eq!(
            detect_polarity("U1", "Package_SO:SOIC-8"),
            Some(Polarity::Pin1)
        );
        assert_eq!(detect_polarity("R1", "Resistor_SMD:R_0805"), None);
    }
}
