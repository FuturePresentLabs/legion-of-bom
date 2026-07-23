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

use std::collections::BTreeMap;

use crate::sexpr::Sexpr;
use crate::source::CircuitSource;

/// A placed component: board-space centre + pad bounding box (mm).
#[derive(Debug, Clone)]
pub struct PlacedPart {
    pub refdes: String,
    pub value: String,
    pub cx: f64,
    pub cy: f64,
    pub bbox: (f64, f64, f64, f64),
    pub back: bool,
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

/// A build-step kind, in low-profile-first assembly order (DESIGN 7.8).
struct Kind {
    prefix: &'static str,
    title: &'static str,
    caution: Option<&'static str>,
}

// Order matters — this *is* the build sequence.
const KINDS: &[Kind] = &[
    Kind {
        prefix: "R",
        title: "Resistors",
        caution: None,
    },
    Kind {
        prefix: "D",
        title: "Diodes & LEDs",
        caution: Some("Polarity: match the cathode band / LED flat to the silkscreen."),
    },
    Kind {
        prefix: "J",
        title: "Connectors, sockets & headers",
        caution: Some("Orientation: pin 1 to the silkscreen mark."),
    },
    Kind {
        prefix: "SW",
        title: "Switches",
        caution: None,
    },
    Kind {
        prefix: "C",
        title: "Capacitors",
        caution: Some(
            "Polarity: electrolytic/tantalum caps have a + / stripe — check before soldering.",
        ),
    },
    Kind {
        prefix: "Q",
        title: "Transistors",
        caution: Some("Orientation: match the flat/tab to the silkscreen."),
    },
    Kind {
        prefix: "U",
        title: "ICs",
        caution: Some("Orientation: pin 1 (dot/notch) to the silkscreen mark."),
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

    // Attach the circuit value to each placed part.
    let mut parts: Vec<PlacedPart> = placed
        .into_iter()
        .map(|mut p| {
            if let Some(v) = values.get(p.refdes.as_str()) {
                p.value = v.to_string();
            }
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
        let group = idxs
            .iter()
            .map(|&i| {
                used[i] = true;
                parts[i].clone()
            })
            .collect();
        steps.push(BuildStep {
            title: kind.title.to_string(),
            parts: group,
            caution: kind.caution.map(str::to_string),
        });
    }
    let remaining: Vec<PlacedPart> = parts
        .iter()
        .enumerate()
        .filter(|(i, _)| !used[*i])
        .map(|(_, p)| p.clone())
        .collect();
    if !remaining.is_empty() {
        steps.push(BuildStep {
            title: "Remaining parts".to_string(),
            parts: remaining,
            caution: None,
        });
    }

    Ok(BuildGuide {
        name: circuit.name().to_string(),
        outline: board_outline(board_pcb).unwrap_or((0.0, 0.0, 10.0, 10.0)),
        steps,
    })
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

        // Pad bounding box, in board coordinates (rotation-0 grid placement).
        let mut bb = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
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
        }
        if !bb.0.is_finite() {
            bb = (fx - 0.5, fy - 0.5, fx + 0.5, fy + 0.5);
        }
        parts.push(PlacedPart {
            refdes,
            value: String::new(),
            cx: fx,
            cy: fy,
            bbox: bb,
            back,
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

/// Render the guide as a self-contained HTML page (inline CSS + one top-down SVG
/// per step, the step's parts highlighted).
pub fn guide_to_html(guide: &BuildGuide) -> String {
    let total = guide.steps.len();
    let mut body = String::new();
    body.push_str(&format!(
        "<h1>Build guide — {}</h1><p class=\"sub\">{total} steps, low-profile first. \
         Highlighted parts are for the current step.</p>",
        esc(&guide.name)
    ));
    for (i, step) in guide.steps.iter().enumerate() {
        let highlight: std::collections::HashSet<&str> =
            step.parts.iter().map(|p| p.refdes.as_str()).collect();
        let svg = board_svg(guide, &highlight);
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
.caution{background:#fff6e0;border-left:3px solid #e0a800;padding:.5rem .7rem;border-radius:4px;margin:.6rem 0 0}";

/// A top-down SVG of the board: outline + every part as a box, `highlight`ed
/// parts filled red, the rest greyed.
fn board_svg(guide: &BuildGuide, highlight: &std::collections::HashSet<&str>) -> String {
    let (x0, y0, x1, y1) = guide.outline;
    let (w, h) = (x1 - x0, y1 - y0);
    let pad = 2.0;
    let mut svg = format!(
        "<svg viewBox=\"{} {} {} {}\" xmlns=\"http://www.w3.org/2000/svg\">",
        x0 - pad,
        y0 - pad,
        w + 2.0 * pad,
        h + 2.0 * pad
    );
    svg.push_str(&format!(
        "<rect x=\"{x0}\" y=\"{y0}\" width=\"{w}\" height=\"{h}\" fill=\"#eef3ee\" \
         stroke=\"#3a6\" stroke-width=\"0.3\"/>"
    ));
    // Collect all parts once (union across steps) so context boxes show greyed.
    let all: Vec<&PlacedPart> = guide.steps.iter().flat_map(|s| &s.parts).collect();
    let fs = (w.min(h) / 30.0).clamp(0.7, 2.0);
    for p in all {
        let (bx0, by0, bx1, by1) = p.bbox;
        let on = highlight.contains(p.refdes.as_str());
        let (fill, stroke, tw) = if on {
            ("#e34a4a", "#a11", 0.35)
        } else {
            ("#dcdcdc", "#aaa", 0.15)
        };
        svg.push_str(&format!(
            "<rect x=\"{bx0}\" y=\"{by0}\" width=\"{}\" height=\"{}\" rx=\"0.2\" \
             fill=\"{fill}\" fill-opacity=\"{}\" stroke=\"{stroke}\" stroke-width=\"{tw}\"/>",
            bx1 - bx0,
            by1 - by0,
            if on { 0.85 } else { 0.5 },
        ));
        if on {
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{fs}\" text-anchor=\"middle\" \
                 dominant-baseline=\"middle\" fill=\"#fff\" font-weight=\"bold\">{}</text>",
                p.cx,
                p.cy,
                esc(&p.refdes)
            ));
        }
    }
    svg.push_str("</svg>");
    svg
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
        let html = guide_to_html(&g);
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
}
