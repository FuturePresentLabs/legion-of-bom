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
}

/// A part positioned on the sheet.
struct Placed {
    refdes: String,
    value: String,
    col: usize,
    row: usize,
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
    let mut by_col: HashMap<usize, Vec<(&str, &str)>> = HashMap::new();
    for p in circuit.parts() {
        let c = rank.get(&p.refdes.0).copied().unwrap_or(0);
        by_col
            .entry(c)
            .or_default()
            .push((p.refdes.0.as_str(), p.value.as_str()));
    }
    let mut out = Vec::new();
    let mut cols: Vec<usize> = by_col.keys().copied().collect();
    cols.sort_unstable();
    for (ci, c) in cols.iter().enumerate() {
        let mut parts = by_col.remove(c).unwrap_or_default();
        parts.sort_by(|a, b| a.0.cmp(b.0));
        for (ri, (refdes, value)) in parts.into_iter().enumerate() {
            out.push(Placed {
                refdes: refdes.to_string(),
                value: value.to_string(),
                col: ci,
                row: ri,
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
         <rect width=\"{w:.0}\" height=\"{h:.0}\" fill=\"#fbfbf7\"/>",
        xml_escape(circuit.name()),
    ));

    // Signal nets first, so wires sit behind the part boxes. Nets that start in the
    // same column would otherwise share one trunk line — and stack their labels on
    // top of each other — so each column's channel is divided into lanes.
    let ink = "#2a2a28";
    let mut routed: Vec<(&str, Vec<&Placed>)> = Vec::new();
    for net in circuit.nets() {
        if is_power(&net.name) {
            continue;
        }
        let mut pts: Vec<&Placed> = net
            .pins
            .iter()
            .filter_map(|p| pos.get(p.refdes.0.as_str()).copied())
            .collect();
        pts.sort_by_key(|p| (p.col, p.row));
        pts.dedup_by(|a, b| a.refdes == b.refdes);
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
            *counts.entry(pts[0].col).or_default() += 1;
        }
        for (_, pts) in &routed {
            let c = pts[0].col;
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
            pts[0].x() + sheet::BOX_W + channel * (lane as f64 + 1.0) / (lanes as f64 + 1.0);
        let (y0, y1) = pts.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| {
            (lo.min(p.cy()), hi.max(p.cy()))
        });
        s.push_str(&format!(
            "<path d=\"M{trunk:.1} {y0:.1} L{trunk:.1} {y1:.1}\" stroke=\"{ink}\" \
             stroke-width=\"1.3\" fill=\"none\"/>"
        ));
        for p in pts.iter() {
            // Leave the box on the side the trunk is on.
            let from = if p.x() + sheet::BOX_W <= trunk {
                p.x() + sheet::BOX_W
            } else {
                p.x()
            };
            s.push_str(&format!(
                "<path d=\"M{from:.1} {:.1} L{trunk:.1} {:.1}\" stroke=\"{ink}\" \
                 stroke-width=\"1.3\" fill=\"none\"/>",
                p.cy(),
                p.cy()
            ));
        }
        // Net label at the top of the trunk, on a small backing so it stays legible
        // where it crosses a wire.
        let label = ellipsize(name, 14);
        let lw = label.chars().count() as f64 * 6.0 + 6.0;
        s.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{lw:.1}\" height=\"12\" fill=\"#fbfbf7\"/>\
             <text x=\"{:.1}\" y=\"{:.1}\" font-family=\"ui-monospace,monospace\" \
             font-size=\"10\" fill=\"#6b7280\" text-anchor=\"middle\">{}</text>",
            trunk - lw / 2.0,
            y0 - 16.0,
            trunk,
            y0 - 6.0,
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

    // Part boxes on top.
    for p in &placed {
        s.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.0}\" height=\"{:.0}\" rx=\"4\" \
             fill=\"#ffffff\" stroke=\"{ink}\" stroke-width=\"1.4\"/>",
            p.x(),
            p.y(),
            sheet::BOX_W,
            sheet::BOX_H
        ));
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"ui-monospace,monospace\" \
             font-size=\"13\" font-weight=\"600\" fill=\"{ink}\" text-anchor=\"middle\">{}</text>",
            p.cx(),
            p.cy() - 2.0,
            xml_escape(&p.refdes)
        ));
        if !p.value.is_empty() {
            let value = ellipsize(&p.value, 16);
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"ui-monospace,monospace\" \
                 font-size=\"10\" fill=\"#6b7280\" text-anchor=\"middle\">{}</text>",
                p.cx(),
                p.cy() + 13.0,
                xml_escape(&value)
            ));
        }
    }

    s.push_str("</svg>");
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
