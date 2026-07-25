//! Schematic rendering — a readable circuit diagram straight from the model (agr).
//!
//! Drawn here rather than shelled out: SKiDL's own schematic generator is
//! experimental, and the alternatives (netlistsvg, graphviz) are external tools the
//! dashboard can't rely on. Working from [`CircuitSource`] instead keeps this
//! DSL-agnostic (DESIGN 2.3/3.3) — any circuit producer gets a diagram — and makes
//! it as cheap and always-available as [`crate::panel::panel_to_svg`].
//!
//! Layout follows how a person reads a circuit: signal flows left→right, so parts
//! are ranked by their distance from the input net and stacked in columns. Signal
//! nets route through a vertical trunk between columns. Power and ground are drawn
//! as rail stubs at each part rather than wires — standard schematic practice, and
//! the thing that keeps a diagram legible, since a rail touches nearly everything.

use std::collections::{HashMap, VecDeque};

use crate::source::CircuitSource;
use crate::symbols::{read_symbol_graphics, SymFill, SymShape, SymbolGraphics};

/// The sheet colour — also what a `background`-filled symbol shape paints with.
const SHEET_BG: &str = "#fbfbf7";

/// Sheet geometry (px). The diagram is emitted at these units and scaled by the
/// viewer's `viewBox`.
mod sheet {
    pub const COL_W: f64 = 200.0;
    /// Tall enough that a row's rail stubs (up) clear the row above's ground
    /// stubs (down) — both carry a symbol and a label.
    pub const ROW_H: f64 = 118.0;
    pub const BOX_W: f64 = 108.0;
    pub const BOX_H: f64 = 46.0;
    pub const MARGIN: f64 = 40.0;
    /// Extra room on the right for a trunk + net label hanging off the last column.
    pub const GUTTER: f64 = 70.0;
    /// Preferred symbol scale, and the slot a symbol is fitted into.
    pub const SYM_PX_PER_MM: f64 = 6.5;
    pub const SYM_MAX_W: f64 = 104.0;
    pub const SYM_MAX_H: f64 = 74.0;
}

/// A part awaiting placement: refdes, value, and its resolved symbol.
type PendingPart<'a> = (&'a str, &'a str, Option<SymbolGraphics>);

/// Where a net attaches to one part: the part, and the point in sheet px.
type Attach<'a> = (&'a Placed, (f64, f64));

/// A part positioned on the sheet, with its KiCad symbol when one resolved.
struct Placed {
    refdes: String,
    value: String,
    col: usize,
    row: usize,
    /// The drawn body from the KiCad symbol library. `None` — no library, unknown
    /// symbol, or a multi-unit part — falls back to a labelled box.
    sym: Option<SymbolGraphics>,
}

impl Placed {
    fn x(&self) -> f64 {
        sheet::MARGIN + self.col as f64 * sheet::COL_W
    }
    fn y(&self) -> f64 {
        sheet::MARGIN + self.row as f64 * sheet::ROW_H
    }
    fn cx(&self) -> f64 {
        self.x() + sheet::BOX_W / 2.0
    }
    fn cy(&self) -> f64 {
        self.y() + sheet::BOX_H / 2.0
    }

    /// Symbol-space → sheet-px transform: `(scale, bcx, bcy)`, sized so the symbol
    /// fits its slot without being blown up past [`sheet::SYM_PX_PER_MM`].
    fn sym_fit(&self) -> Option<(f64, f64, f64)> {
        let g = self.sym.as_ref()?;
        let (x0, y0, x1, y1) = g.bounds();
        let (bw, bh) = ((x1 - x0).max(0.1), (y1 - y0).max(0.1));
        let scale = (sheet::SYM_MAX_W / bw)
            .min(sheet::SYM_MAX_H / bh)
            .min(sheet::SYM_PX_PER_MM);
        Some((scale, (x0 + x1) / 2.0, (y0 + y1) / 2.0))
    }

    /// A symbol point in sheet px. KiCad symbol space is Y-**up**, the sheet Y-down.
    fn sym_px(&self, x: f64, y: f64) -> (f64, f64) {
        match self.sym_fit() {
            Some((s, bcx, bcy)) => (self.cx() + (x - bcx) * s, self.cy() - (y - bcy) * s),
            None => (self.cx(), self.cy()),
        }
    }

    /// Where a wire should attach for this part's `pin` — the pin's tip when the
    /// symbol resolved, else the box edge.
    fn pin_anchor(&self, pin: &str) -> Option<(f64, f64)> {
        let g = self.sym.as_ref()?;
        let p = g.pins.iter().find(|p| p.number == pin)?;
        let (tx, ty) = p.tip();
        Some(self.sym_px(tx, ty))
    }
}

/// True for a ground net.
fn is_ground(name: &str) -> bool {
    let u = name.trim().to_ascii_uppercase();
    matches!(u.as_str(), "GND" | "GNDA" | "AGND" | "DGND" | "VSS" | "0") || u.ends_with("GND")
}

/// True for a supply rail (but not ground).
fn is_rail(name: &str) -> bool {
    let u = name.trim().to_ascii_uppercase();
    !is_ground(name)
        && (u.starts_with('+')
            || u.starts_with('-')
            || matches!(u.as_str(), "VCC" | "VDD" | "VEE" | "V+" | "V-"))
}

/// Power and ground are drawn as stubs, not routed wires.
fn is_power(name: &str) -> bool {
    is_ground(name) || is_rail(name)
}

/// Rank parts left→right by hops from the input net over signal nets, so the
/// diagram reads the way the signal flows. Parts the search never reaches (an
/// isolated decoupling cap, say) land after the ones it did.
fn rank_parts(circuit: &dyn CircuitSource) -> HashMap<String, usize> {
    // Signal-net adjacency: power rails would make everything adjacent to
    // everything, so they're excluded.
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for net in circuit.nets() {
        if is_power(&net.name) {
            continue;
        }
        let refs: Vec<&str> = {
            let mut r: Vec<&str> = net.pins.iter().map(|p| p.refdes.0.as_str()).collect();
            r.sort_unstable();
            r.dedup();
            r
        };
        for &a in &refs {
            for &b in &refs {
                if a != b {
                    adj.entry(a).or_default().push(b);
                }
            }
        }
    }

    // Seed from whatever looks like the input; else the alphabetically first part,
    // so the layout is deterministic either way.
    let input_like = ["IN", "SIG_IN", "INPUT", "AUDIO_IN", "IN_L"];
    let mut seeds: Vec<&str> = circuit
        .nets()
        .iter()
        .filter(|n| input_like.iter().any(|c| n.name.eq_ignore_ascii_case(c)))
        .flat_map(|n| n.pins.iter().map(|p| p.refdes.0.as_str()))
        .collect();
    seeds.sort_unstable();
    seeds.dedup();
    if seeds.is_empty() {
        let mut all: Vec<&str> = circuit
            .parts()
            .iter()
            .map(|p| p.refdes.0.as_str())
            .collect();
        all.sort_unstable();
        seeds.extend(all.first().copied());
    }

    let mut rank: HashMap<String, usize> = HashMap::new();
    let mut queue: VecDeque<(&str, usize)> = VecDeque::new();
    for s in &seeds {
        rank.insert(s.to_string(), 0);
        queue.push_back((s, 0));
    }
    while let Some((r, d)) = queue.pop_front() {
        let mut next: Vec<&str> = adj.get(r).cloned().unwrap_or_default();
        next.sort_unstable();
        next.dedup();
        for n in next {
            if !rank.contains_key(n) {
                rank.insert(n.to_string(), d + 1);
                queue.push_back((n, d + 1));
            }
        }
    }
    // Anything unreached goes in a final column.
    let max = rank.values().copied().max().unwrap_or(0);
    for p in circuit.parts() {
        rank.entry(p.refdes.0.clone()).or_insert(max + 1);
    }
    rank
}

/// Lay parts out in columns by rank, stacked in refdes order within a column.
fn layout(circuit: &dyn CircuitSource) -> Vec<Placed> {
    let rank = rank_parts(circuit);
    // Resolve each part's KiCad symbol once, cached by `lib:part` since a circuit
    // reuses the same handful. Multi-unit parts (an op-amp is two amplifiers plus a
    // power unit) are declined here: drawing them properly means splitting their
    // pins across separately-placed units, so they fall back to a box.
    let dir = crate::skidl::kicad_symbol_dir();
    let mut cache: HashMap<String, Option<SymbolGraphics>> = HashMap::new();
    let mut symbol_for = |lib_part: Option<&str>| -> Option<SymbolGraphics> {
        let dir = dir.as_ref()?;
        let key = lib_part?;
        let (lib, part) = key.split_once(':')?;
        cache
            .entry(key.to_string())
            .or_insert_with(|| read_symbol_graphics(dir.path(), lib, part).filter(|g| g.units == 1))
            .clone()
    };

    let mut by_col: HashMap<usize, Vec<PendingPart>> = HashMap::new();
    for p in circuit.parts() {
        let c = rank.get(&p.refdes.0).copied().unwrap_or(0);
        let sym = symbol_for(p.library_part.as_deref());
        by_col
            .entry(c)
            .or_default()
            .push((p.refdes.0.as_str(), p.value.as_str(), sym));
    }
    let mut out = Vec::new();
    let mut cols: Vec<usize> = by_col.keys().copied().collect();
    cols.sort_unstable();
    for (ci, c) in cols.iter().enumerate() {
        let mut parts = by_col.remove(c).unwrap_or_default();
        parts.sort_by(|a, b| a.0.cmp(b.0));
        for (ri, (refdes, value, sym)) in parts.into_iter().enumerate() {
            out.push(Placed {
                refdes: refdes.to_string(),
                value: value.to_string(),
                col: ci,
                row: ri,
                sym,
            });
        }
    }
    out
}

/// Render the circuit as a standalone SVG schematic diagram.
pub fn schematic_to_svg(circuit: &dyn CircuitSource) -> String {
    let placed = layout(circuit);
    let pos: HashMap<&str, &Placed> = placed.iter().map(|p| (p.refdes.as_str(), p)).collect();

    let cols = placed.iter().map(|p| p.col).max().unwrap_or(0) + 1;
    let rows = placed.iter().map(|p| p.row).max().unwrap_or(0) + 1;
    let w = 2.0 * sheet::MARGIN + cols as f64 * sheet::COL_W + sheet::GUTTER;
    let h = 2.0 * sheet::MARGIN + rows as f64 * sheet::ROW_H;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w:.0} {h:.0}\" \
         width=\"{w:.0}\" height=\"{h:.0}\" role=\"img\" aria-label=\"{} schematic\">\
         <rect width=\"{w:.0}\" height=\"{h:.0}\" fill=\"{SHEET_BG}\"/>",
        xml_escape(circuit.name()),
    ));

    // Signal nets first, so wires sit behind the part boxes. Nets that start in the
    // same column would otherwise share one trunk line — and stack their labels on
    // top of each other — so each column's channel is divided into lanes.
    let ink = "#2a2a28";
    // Each connection is a (part, attach-point): the pin's own tip when the symbol
    // resolved, else the middle of the fallback box.
    let mut routed: Vec<(&str, Vec<Attach>)> = Vec::new();
    for net in circuit.nets() {
        if is_power(&net.name) {
            continue;
        }
        let mut pts: Vec<Attach> = Vec::new();
        for pin in &net.pins {
            let Some(p) = pos.get(pin.refdes.0.as_str()).copied() else {
                continue;
            };
            if pts.iter().any(|(q, _)| q.refdes == p.refdes) {
                continue; // one attach point per part is enough to read
            }
            let anchor = p.pin_anchor(&pin.pin).unwrap_or((p.cx(), p.cy()));
            pts.push((p, anchor));
        }
        pts.sort_by_key(|(p, _)| (p.col, p.row));
        if pts.len() >= 2 {
            routed.push((net.name.as_str(), pts));
        }
    }
    // Lane assignment: index within the group of nets leaving the same column.
    let mut lane_of: Vec<(usize, usize)> = Vec::with_capacity(routed.len()); // (lane, lanes)
    {
        let mut per_col: HashMap<usize, usize> = HashMap::new();
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for (_, pts) in &routed {
            *counts.entry(pts[0].0.col).or_default() += 1;
        }
        for (_, pts) in &routed {
            let c = pts[0].0.col;
            let i = per_col.entry(c).or_insert(0);
            lane_of.push((*i, counts[&c]));
            *i += 1;
        }
    }

    for (idx, (name, pts)) in routed.iter().enumerate() {
        let (lane, lanes) = lane_of[idx];
        // Spread trunks evenly across the channel between this column's boxes and
        // the next, so parallel nets stay visually distinct.
        let channel = sheet::COL_W - sheet::BOX_W;
        let trunk =
            pts[0].0.x() + sheet::BOX_W + channel * (lane as f64 + 1.0) / (lanes as f64 + 1.0);
        let (y0, y1) = pts.iter().fold((f64::MAX, f64::MIN), |(lo, hi), (_, a)| {
            (lo.min(a.1), hi.max(a.1))
        });
        s.push_str(&format!(
            "<path d=\"M{trunk:.1} {y0:.1} L{trunk:.1} {y1:.1}\" stroke=\"{ink}\" \
             stroke-width=\"1.3\" fill=\"none\"/>"
        ));
        for (_, (ax, ay)) in pts.iter() {
            s.push_str(&format!(
                "<path d=\"M{ax:.1} {ay:.1} L{trunk:.1} {ay:.1}\" stroke=\"{ink}\" \
                 stroke-width=\"1.3\" fill=\"none\"/>"
            ));
        }
        // Net label at the top of the trunk, on a small backing so it stays legible
        // where it crosses a wire.
        let label = ellipsize(name, 14);
        let lw = label.chars().count() as f64 * 6.0 + 6.0;
        // Stagger by lane as well as by x: two nets leaving the same column at the
        // same height would otherwise print their labels on top of each other.
        let ly = y0 - 6.0 - lane as f64 * 12.0;
        s.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{lw:.1}\" height=\"12\" fill=\"{SHEET_BG}\"/>\
             <text x=\"{:.1}\" y=\"{:.1}\" font-family=\"ui-monospace,monospace\" \
             font-size=\"10\" fill=\"#6b7280\" text-anchor=\"middle\">{}</text>",
            trunk - lw / 2.0,
            ly - 10.0,
            trunk,
            ly,
            xml_escape(&label)
        ));
    }

    // Power / ground stubs, per part, so rails never cross the sheet.
    let mut stubs: HashMap<&str, Vec<&str>> = HashMap::new();
    for net in circuit.nets() {
        if !is_power(&net.name) {
            continue;
        }
        for pin in &net.pins {
            let e = stubs.entry(pin.refdes.0.as_str()).or_default();
            if !e.contains(&net.name.as_str()) {
                e.push(net.name.as_str());
            }
        }
    }
    for p in &placed {
        let Some(nets) = stubs.get(p.refdes.as_str()) else {
            continue;
        };
        for (i, n) in nets.iter().enumerate() {
            let up = is_rail(n);
            let x = p.x() + 22.0 + i as f64 * 34.0;
            let (y_from, y_to) = if up {
                (p.y(), p.y() - 15.0)
            } else {
                (p.y() + sheet::BOX_H, p.y() + sheet::BOX_H + 15.0)
            };
            s.push_str(&format!(
                "<path d=\"M{x:.1} {y_from:.1} L{x:.1} {y_to:.1}\" stroke=\"{ink}\" \
                 stroke-width=\"1.2\" fill=\"none\"/>"
            ));
            if up {
                // Rail bar.
                s.push_str(&format!(
                    "<path d=\"M{:.1} {y_to:.1} L{:.1} {y_to:.1}\" stroke=\"{ink}\" \
                     stroke-width=\"1.6\"/>",
                    x - 7.0,
                    x + 7.0
                ));
            } else {
                // Ground triangle.
                for (k, half) in [(0.0, 7.0), (2.5, 4.5), (5.0, 2.0)] {
                    s.push_str(&format!(
                        "<path d=\"M{:.1} {:.1} L{:.1} {:.1}\" stroke=\"{ink}\" \
                         stroke-width=\"1.4\"/>",
                        x - half,
                        y_to + k,
                        x + half,
                        y_to + k
                    ));
                }
            }
            s.push_str(&format!(
                "<text x=\"{x:.1}\" y=\"{:.1}\" font-family=\"ui-monospace,monospace\" \
                 font-size=\"9\" fill=\"#6b7280\" text-anchor=\"middle\">{}</text>",
                if up { y_to - 5.0 } else { y_to + 22.0 },
                xml_escape(n)
            ));
        }
    }

    // Parts on top: the real KiCad symbol where one resolved, else a labelled box.
    for p in &placed {
        let mut label_y = p.cy() - 2.0;
        let mut value_y = p.cy() + 13.0;
        match p.sym.as_ref() {
            Some(g) => {
                s.push_str(&symbol_svg(p, g, ink));
                // Caption below the symbol, clear of its pins.
                let (_, _, _, y1) = g.bounds();
                let (_, top) = p.sym_px(0.0, y1);
                let (_, bottom) = p.sym_px(0.0, g.bounds().1);
                label_y = bottom.max(top) + 13.0;
                value_y = label_y + 12.0;
            }
            None => {
                s.push_str(&format!(
                    "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.0}\" height=\"{:.0}\" rx=\"4\" \
                     fill=\"#ffffff\" stroke=\"{ink}\" stroke-width=\"1.4\"/>",
                    p.x(),
                    p.y(),
                    sheet::BOX_W,
                    sheet::BOX_H
                ));
            }
        }
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{label_y:.1}\" font-family=\"ui-monospace,monospace\" \
             font-size=\"13\" font-weight=\"600\" fill=\"{ink}\" text-anchor=\"middle\">{}</text>",
            p.cx(),
            xml_escape(&p.refdes)
        ));
        if !p.value.is_empty() {
            let value = ellipsize(&p.value, 16);
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{value_y:.1}\" font-family=\"ui-monospace,monospace\" \
                 font-size=\"10\" fill=\"#6b7280\" text-anchor=\"middle\">{}</text>",
                p.cx(),
                xml_escape(&value)
            ));
        }
    }

    s.push_str("</svg>");
    s
}

/// Draw a resolved KiCad symbol: its body primitives plus a lead for every pin,
/// mapped from symbol space (mm, Y up) into sheet px.
fn symbol_svg(p: &Placed, g: &SymbolGraphics, ink: &str) -> String {
    let mut s = String::new();
    // KiCad fill modes: `outline` is solid in the line colour, `background` is the
    // sheet colour (it occludes, it doesn't go black), `none` is open.
    let paint = |f: SymFill| match f {
        SymFill::Outline => ink,
        SymFill::Background => SHEET_BG,
        SymFill::None => "none",
    };
    let pt = |x: f64, y: f64| p.sym_px(x, y);

    for shape in &g.shapes {
        match shape {
            SymShape::Rect {
                x0,
                y0,
                x1,
                y1,
                fill,
            } => {
                let (ax, ay) = pt(*x0, *y0);
                let (bx, by) = pt(*x1, *y1);
                s.push_str(&format!(
                    "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" \
                     fill=\"{}\" stroke=\"{ink}\" stroke-width=\"1.4\"/>",
                    ax.min(bx),
                    ay.min(by),
                    (bx - ax).abs(),
                    (by - ay).abs(),
                    paint(*fill),
                ));
            }
            SymShape::Poly { pts, fill } => {
                let d: Vec<String> = pts
                    .iter()
                    .map(|&(x, y)| {
                        let (px, py) = pt(x, y);
                        format!("{px:.1},{py:.1}")
                    })
                    .collect();
                s.push_str(&format!(
                    "<polyline points=\"{}\" fill=\"{}\" stroke=\"{ink}\" \
                     stroke-width=\"1.4\" stroke-linejoin=\"round\"/>",
                    d.join(" "),
                    paint(*fill)
                ));
            }
            SymShape::Circle { cx, cy, r, fill } => {
                let (px, py) = pt(*cx, *cy);
                let rr = r * p.sym_fit().map(|(sc, _, _)| sc).unwrap_or(1.0);
                s.push_str(&format!(
                    "<circle cx=\"{px:.1}\" cy=\"{py:.1}\" r=\"{rr:.1}\" fill=\"{}\" \
                     stroke=\"{ink}\" stroke-width=\"1.4\"/>",
                    paint(*fill)
                ));
            }
            SymShape::Arc { start, mid, end } => {
                // Three-point arc → SVG arc. The radius comes from the
                // circumcircle of the three points; degenerate (collinear) cases
                // fall back to a straight line, which is what they look like.
                let (ax, ay) = pt(start.0, start.1);
                let (mx, my) = pt(mid.0, mid.1);
                let (bx, by) = pt(end.0, end.1);
                let d = 2.0 * (ax * (my - by) + mx * (by - ay) + bx * (ay - my));
                if d.abs() < 1e-6 {
                    s.push_str(&format!(
                        "<path d=\"M{ax:.1} {ay:.1} L{bx:.1} {by:.1}\" fill=\"none\" \
                         stroke=\"{ink}\" stroke-width=\"1.4\"/>"
                    ));
                    continue;
                }
                let ux = ((ax * ax + ay * ay) * (my - by)
                    + (mx * mx + my * my) * (by - ay)
                    + (bx * bx + by * by) * (ay - my))
                    / d;
                let uy = ((ax * ax + ay * ay) * (bx - mx)
                    + (mx * mx + my * my) * (ax - bx)
                    + (bx * bx + by * by) * (mx - ax))
                    / d;
                let r = ((ax - ux).powi(2) + (ay - uy).powi(2)).sqrt();
                // Sweep direction from the sign of the cross product at the mid point.
                let sweep = if (mx - ax) * (by - ay) - (my - ay) * (bx - ax) > 0.0 {
                    1
                } else {
                    0
                };
                s.push_str(&format!(
                    "<path d=\"M{ax:.1} {ay:.1} A{r:.1} {r:.1} 0 0 {sweep} {bx:.1} {by:.1}\" \
                     fill=\"none\" stroke=\"{ink}\" stroke-width=\"1.4\"/>"
                ));
            }
        }
    }

    // Pin leads: body root out to the tip a wire attaches at.
    for pin in &g.pins {
        let (rx, ry) = pt(pin.x, pin.y);
        let (tx, ty) = pin.tip();
        let (tx, ty) = pt(tx, ty);
        s.push_str(&format!(
            "<path d=\"M{rx:.1} {ry:.1} L{tx:.1} {ty:.1}\" stroke=\"{ink}\" \
             stroke-width=\"1.2\" fill=\"none\"/>"
        ));
    }
    s
}

/// Shorten a string to `max` chars with an ellipsis, so a long value (a full
/// footprint name, say) can't spill out of its part box.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};

    fn demo() -> Circuit {
        let mut c = Circuit::new("demo");
        c.parts = vec![
            Part::new("J1", "in"),
            Part::new("R1", "10k"),
            Part::new("U1", "TL072"),
            Part::new("J2", "out"),
            Part::new("C1", "100nF"),
        ];
        c.nets = vec![
            Net::new("IN", vec![PinRef::new("J1", "T"), PinRef::new("R1", "1")]),
            Net::new("MID", vec![PinRef::new("R1", "2"), PinRef::new("U1", "3")]),
            Net::new("OUT", vec![PinRef::new("U1", "1"), PinRef::new("J2", "T")]),
            Net::new("+12V", vec![PinRef::new("U1", "8"), PinRef::new("C1", "1")]),
            Net::new("GND", vec![PinRef::new("J1", "S"), PinRef::new("C1", "2")]),
        ];
        c
    }

    #[test]
    fn ranks_parts_by_signal_flow_from_the_input() {
        let c = demo();
        let r = rank_parts(&c);
        // J1 is on IN, so it seeds at 0 and the chain fans out left→right.
        assert_eq!(r["J1"], 0);
        assert!(r["R1"] < r["U1"], "R1 should sit left of U1");
        assert!(r["U1"] < r["J2"], "U1 should sit left of the output jack");
    }

    /// Power rails must not drive the ranking — a rail touches nearly every part,
    /// so including it would collapse the whole circuit into one column.
    #[test]
    fn power_nets_do_not_collapse_the_layout() {
        let c = demo();
        let r = rank_parts(&c);
        let cols: std::collections::HashSet<usize> = c
            .parts
            .iter()
            .map(|p| r[p.refdes.0.as_str()])
            .collect::<std::collections::HashSet<_>>();
        assert!(cols.len() >= 3, "expected several columns, got {cols:?}");
    }

    #[test]
    fn svg_is_well_formed_and_labels_every_part() {
        let c = demo();
        let svg = schematic_to_svg(&c);
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        for p in &c.parts {
            assert!(
                svg.contains(&format!(">{}<", p.refdes.0)),
                "missing {}",
                p.refdes.0
            );
        }
        // Rails are drawn as stubs, not routed as trunk wires.
        assert!(svg.contains("+12V") && svg.contains("GND"));
    }

    #[test]
    fn handles_a_circuit_with_no_recognisable_input() {
        let mut c = Circuit::new("x");
        c.parts = vec![Part::new("R1", "1k"), Part::new("R2", "2k")];
        c.nets = vec![Net::new(
            "N$1",
            vec![PinRef::new("R1", "2"), PinRef::new("R2", "1")],
        )];
        let svg = schematic_to_svg(&c);
        assert!(svg.contains(">R1<") && svg.contains(">R2<"));
    }
}
