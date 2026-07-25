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
    /// A row has to hold, top to bottom: the rail stub and its label reaching up,
    /// the symbol body, the ground stub and its label reaching down, and then the
    /// refdes/value caption under all of it — about 30 + 74 + 33 + 26 px. Skimp and
    /// a cap's caption lands on the next part's supply rail.
    pub const ROW_H: f64 = 175.0;
    pub const BOX_W: f64 = 108.0;
    pub const BOX_H: f64 = 46.0;
    pub const MARGIN: f64 = 40.0;
    /// Extra room on the right for a trunk + net label hanging off the last column.
    pub const GUTTER: f64 = 70.0;
    /// Preferred symbol scale, and the slot a symbol is fitted into.
    pub const SYM_PX_PER_MM: f64 = 6.5;
    pub const SYM_MAX_W: f64 = 104.0;
    pub const SYM_MAX_H: f64 = 74.0;
    /// How far a wire runs out along its pin before turning toward the trunk.
    pub const PIN_STUB: f64 = 10.0;
    /// How far a power/ground stub runs out from its pin before its glyph.
    pub const RAIL_STUB: f64 = 13.0;
    /// Sheet frame inset, and the title block that sits in its bottom-right corner.
    pub const FRAME: f64 = 12.0;
    pub const TITLE_W: f64 = 300.0;
    pub const TITLE_H: f64 = 64.0;
}

/// A part awaiting placement: refdes, value, and its resolved symbol.
type PendingPart<'a> = (&'a str, &'a str, Option<SymbolGraphics>, Vec<String>);

/// Where a net attaches to one part: the part, the pin's connection point, and
/// the breakout point a short way out along the pin.
type Attach<'a> = (&'a Placed, (f64, f64), (f64, f64));

/// A part positioned on the sheet, with its KiCad symbol when one resolved.
struct Placed {
    refdes: String,
    value: String,
    col: usize,
    row: usize,
    /// The drawn body from the KiCad symbol library. `None` — no library, unknown
    /// symbol, or a multi-unit part — falls back to a labelled box.
    sym: Option<SymbolGraphics>,
    /// Pin identifiers used by the box fallback, in netlist order, so a symbol-less
    /// part still has one distinct attach point per pin.
    box_pins: Vec<String>,
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

    /// Where a wire attaches for this part's `pin`, and the sheet-space direction
    /// it should leave along: the pin's connection point and its outward vector.
    ///
    /// A part with no symbol still gets real, distinct pin positions — its pins are
    /// laid down the sides of its box in netlist order — so a boxed multi-unit part
    /// shows which pin is which instead of every wire meeting at one point.
    fn pin_anchor(&self, pin: &str) -> Option<((f64, f64), (f64, f64))> {
        match self.sym.as_ref() {
            Some(g) => {
                let p = g.pins.iter().find(|p| p.number == pin)?;
                let (ox, oy) = p.outward();
                // Symbol space is Y-up, the sheet Y-down, so the vertical flips.
                Some((self.sym_px(p.x, p.y), (ox, -oy)))
            }
            None => {
                let i = self.box_pins.iter().position(|n| n == pin)?;
                let n = self.box_pins.len().max(1);
                // Odd pins left, even pins right, stepping down the box.
                let left = i % 2 == 0;
                let rows = n.div_ceil(2);
                let slot = (i / 2) as f64 + 0.5;
                let y = self.y() + sheet::BOX_H * slot / rows as f64;
                if left {
                    Some(((self.x(), y), (-1.0, 0.0)))
                } else {
                    Some(((self.x() + sheet::BOX_W, y), (1.0, 0.0)))
                }
            }
        }
    }
}

impl Placed {
    /// Where a power/ground stub should attach. On a real symbol that's the pin
    /// itself; on a fallback box, rails go out the top and grounds out the bottom —
    /// how an IC's supplies are drawn anyway — which also keeps them off the sides
    /// where the signal pins live, and stops a left-hand pin firing its rail
    /// symbol off the edge of the sheet.
    fn power_anchor(
        &self,
        pin: &str,
        rail: bool,
        slot: usize,
        of: usize,
    ) -> ((f64, f64), (f64, f64)) {
        if self.sym.is_some() {
            if let Some(a) = self.pin_anchor(pin) {
                return a;
            }
        }
        let x = self.x() + sheet::BOX_W * (slot as f64 + 0.5) / of.max(1) as f64;
        if rail {
            ((x, self.y()), (0.0, -1.0))
        } else {
            ((x, self.y() + sheet::BOX_H), (0.0, 1.0))
        }
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

    // Pins each part actually uses, in netlist order — the box fallback lays these
    // down its sides so every pin still gets its own attach point.
    let mut used_pins: HashMap<&str, Vec<String>> = HashMap::new();
    for net in circuit.nets() {
        for pin in &net.pins {
            let e = used_pins.entry(pin.refdes.0.as_str()).or_default();
            if !e.contains(&pin.pin) {
                e.push(pin.pin.clone());
            }
        }
    }

    let mut by_col: HashMap<usize, Vec<PendingPart>> = HashMap::new();
    for p in circuit.parts() {
        let c = rank.get(&p.refdes.0).copied().unwrap_or(0);
        let sym = symbol_for(p.library_part.as_deref());
        let pins = used_pins
            .get(p.refdes.0.as_str())
            .cloned()
            .unwrap_or_default();
        by_col
            .entry(c)
            .or_default()
            .push((p.refdes.0.as_str(), p.value.as_str(), sym, pins));
    }
    let mut out = Vec::new();
    let mut cols: Vec<usize> = by_col.keys().copied().collect();
    cols.sort_unstable();
    for (ci, c) in cols.iter().enumerate() {
        let mut parts = by_col.remove(c).unwrap_or_default();
        parts.sort_by(|a, b| a.0.cmp(b.0));
        for (ri, (refdes, value, sym, box_pins)) in parts.into_iter().enumerate() {
            out.push(Placed {
                refdes: refdes.to_string(),
                value: value.to_string(),
                col: ci,
                row: ri,
                sym,
                box_pins,
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
    let w = (2.0 * sheet::MARGIN + cols as f64 * sheet::COL_W + sheet::GUTTER)
        .max(sheet::TITLE_W + 2.0 * sheet::FRAME + 40.0);
    // Room under the drawing for the frame and the title block.
    let h = 2.0 * sheet::MARGIN + rows as f64 * sheet::ROW_H + sheet::TITLE_H + sheet::FRAME;

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
            if pts.iter().any(|(q, _, _)| q.refdes == p.refdes) {
                continue; // one attach point per part is enough to read
            }
            // Leave the pin along its own direction before turning toward the
            // trunk, so the wire continues the pin instead of striking it
            // side-on. Parts with no resolved symbol just break out sideways.
            let (at, (dx, dy)) = p.pin_anchor(&pin.pin).unwrap_or_else(|| {
                // No symbol (a multi-unit part keeps its box): leave from the box
                // edge, not its middle, so the wire still starts on the outline.
                ((p.x() + sheet::BOX_W, p.cy()), (1.0, 0.0))
            });
            let breakout = (at.0 + dx * sheet::PIN_STUB, at.1 + dy * sheet::PIN_STUB);
            pts.push((p, at, breakout));
        }
        pts.sort_by_key(|(p, _, _)| (p.col, p.row));
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
        let (y0, y1) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), (_, _, b)| {
                (lo.min(b.1), hi.max(b.1))
            });
        s.push_str(&format!(
            "<path d=\"M{trunk:.1} {y0:.1} L{trunk:.1} {y1:.1}\" stroke=\"{ink}\" \
             stroke-width=\"1.3\" fill=\"none\"/>"
        ));
        for (_, (ax, ay), (bx, by)) in pts.iter() {
            // pin end → out along the pin → across to the trunk.
            s.push_str(&format!(
                "<path d=\"M{ax:.1} {ay:.1} L{bx:.1} {by:.1} L{trunk:.1} {by:.1}\" \
                 stroke=\"{ink}\" stroke-width=\"1.3\" fill=\"none\"/>"
            ));
        }
        // Net label at the top of the trunk, on a small backing so it stays legible
        // where it crosses a wire.
        let label = ellipsize(name, 14);
        let lw = label.chars().count() as f64 * 6.0 + 6.0;
        // Stagger by lane as well as by x: two nets leaving the same column at the
        // same height would otherwise print their labels on top of each other.
        // Never let the stagger push a label off the top of the sheet.
        let ly = (y0 - 6.0 - lane as f64 * 12.0).max(14.0);
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

    // Power / ground: drawn as a rail bar or ground symbol at each connected pin
    // rather than routed across the sheet — standard practice, and the thing that
    // keeps a diagram legible since a rail touches nearly everything. Attaching at
    // the *pin* (not a fixed offset from the box) is what makes a 10-pin header or
    // a jack's sleeve read as actually connected. Records how far each part's
    // drawing extends downward, so captions can clear it.
    // How many rail / ground stubs each part gets, so a box can space them out.
    let mut slot_count: HashMap<(&str, bool), usize> = HashMap::new();
    for net in circuit.nets() {
        if !is_power(&net.name) {
            continue;
        }
        for pin in &net.pins {
            *slot_count
                .entry((pin.refdes.0.as_str(), is_rail(&net.name)))
                .or_default() += 1;
        }
    }
    let mut slots: HashMap<(&str, bool), usize> = HashMap::new();
    let mut lowest: HashMap<&str, f64> = HashMap::new();
    for p in &placed {
        let bottom = match p.sym.as_ref() {
            Some(g) => p.sym_px(0.0, g.bounds().1).1,
            None => p.y() + sheet::BOX_H,
        };
        lowest.insert(p.refdes.as_str(), bottom);
    }
    for net in circuit.nets() {
        if !is_power(&net.name) {
            continue;
        }
        let rail = is_rail(&net.name);
        for pin in &net.pins {
            let Some(p) = pos.get(pin.refdes.0.as_str()).copied() else {
                continue;
            };
            let slot = *slots.entry((pin.refdes.0.as_str(), rail)).or_insert(0);
            slots.insert((pin.refdes.0.as_str(), rail), slot + 1);
            let of = *slot_count.get(&(pin.refdes.0.as_str(), rail)).unwrap_or(&1);
            let (at, dir) = p.power_anchor(&pin.pin, rail, slot, of);
            let (dx, dy) = dir;
            let end = (at.0 + dx * sheet::RAIL_STUB, at.1 + dy * sheet::RAIL_STUB);
            // Perpendicular to the stub, for the glyph's bars.
            let (px, py) = (-dy, dx);
            s.push_str(&format!(
                "<path d=\"M{:.1} {:.1} L{:.1} {:.1}\" stroke=\"{ink}\" \
                 stroke-width=\"1.2\" fill=\"none\"/>",
                at.0, at.1, end.0, end.1
            ));
            if rail {
                // A single bar across the end of the stub.
                s.push_str(&format!(
                    "<path d=\"M{:.1} {:.1} L{:.1} {:.1}\" stroke=\"{ink}\" \
                     stroke-width=\"1.6\"/>",
                    end.0 - px * 7.0,
                    end.1 - py * 7.0,
                    end.0 + px * 7.0,
                    end.1 + py * 7.0
                ));
            } else {
                // Ground: three shortening bars stepping further along the stub, so
                // the glyph points the way the pin does whatever its orientation.
                for (step, half) in [(0.0, 7.0), (2.5, 4.5), (5.0, 2.0)] {
                    let c = (end.0 + dx * step, end.1 + dy * step);
                    s.push_str(&format!(
                        "<path d=\"M{:.1} {:.1} L{:.1} {:.1}\" stroke=\"{ink}\" \
                         stroke-width=\"1.4\"/>",
                        c.0 - px * half,
                        c.1 - py * half,
                        c.0 + px * half,
                        c.1 + py * half
                    ));
                }
            }
            // Label just past the glyph, along the stub.
            let lx = end.0 + dx * 15.0;
            let ly = end.1 + dy * 15.0 + if dy.abs() < 0.5 { 3.0 } else { 0.0 };
            s.push_str(&format!(
                "<text x=\"{lx:.1}\" y=\"{ly:.1}\" font-family=\"ui-monospace,monospace\" \
                 font-size=\"9\" fill=\"#6b7280\" text-anchor=\"middle\">{}</text>",
                xml_escape(&net.name)
            ));
            if let Some(cur) = lowest.get_mut(pin.refdes.0.as_str()) {
                *cur = cur.max(ly + 4.0);
            }
        }
    }

    // Parts on top: the real KiCad symbol where one resolved, else a labelled box.
    for p in &placed {
        // Caption sits below everything this part draws — body *and* any downward
        // ground stub — so a cap's value can't sit on top of its ground symbol.
        let label_y = lowest
            .get(p.refdes.as_str())
            .copied()
            .unwrap_or(p.y() + sheet::BOX_H)
            + 14.0;
        let value_y = label_y + 12.0;
        match p.sym.as_ref() {
            Some(g) => {
                s.push_str(&symbol_svg(p, g, ink));
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
                // Show the pins on the box too, so a boxed part isn't a black hole
                // that every wire disappears into.
                for pin in &p.box_pins {
                    let Some(((ax, ay), (dx, _))) = p.pin_anchor(pin) else {
                        continue;
                    };
                    s.push_str(&format!(
                        "<path d=\"M{ax:.1} {ay:.1} L{:.1} {ay:.1}\" stroke=\"{ink}\" \
                         stroke-width=\"1.2\"/>\
                         <text x=\"{:.1}\" y=\"{:.1}\" font-family=\"ui-monospace,monospace\" \
                         font-size=\"8\" fill=\"#6b7280\" text-anchor=\"{}\">{}</text>",
                        ax + dx * 6.0,
                        ax - dx * 4.0,
                        ay - 3.0,
                        if dx < 0.0 { "start" } else { "end" },
                        xml_escape(pin)
                    ));
                }
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

    s.push_str(&frame_and_title_svg(w, h, circuit, &placed, ink));
    s.push_str("</svg>");
    s
}

/// The sheet frame plus a KiCad-style title block in its bottom-right corner.
/// Deliberately carries only what the model actually knows — the circuit name, the
/// part count, and the tool — rather than inventing a date or a revision, which on
/// a drawing people may print and file would be worse than leaving blank.
fn frame_and_title_svg(
    w: f64,
    h: f64,
    circuit: &dyn CircuitSource,
    placed: &[Placed],
    ink: &str,
) -> String {
    let f = sheet::FRAME;
    let mut s = format!(
        "<rect x=\"{f:.1}\" y=\"{f:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"none\" \
         stroke=\"{ink}\" stroke-width=\"1.4\"/>",
        w - 2.0 * f,
        h - 2.0 * f
    );
    let (tx, ty) = (w - f - sheet::TITLE_W, h - f - sheet::TITLE_H);
    s.push_str(&format!(
        "<rect x=\"{tx:.1}\" y=\"{ty:.1}\" width=\"{:.1}\" height=\"{:.1}\" \
         fill=\"{SHEET_BG}\" stroke=\"{ink}\" stroke-width=\"1.4\"/>",
        sheet::TITLE_W,
        sheet::TITLE_H
    ));
    // Divider under the title line.
    s.push_str(&format!(
        "<path d=\"M{tx:.1} {:.1} L{:.1} {:.1}\" stroke=\"{ink}\" stroke-width=\"1.0\"/>",
        ty + 30.0,
        tx + sheet::TITLE_W,
        ty + 30.0
    ));
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"ui-monospace,monospace\" \
         font-size=\"15\" font-weight=\"600\" fill=\"{ink}\">{}</text>",
        tx + 10.0,
        ty + 21.0,
        xml_escape(circuit.name())
    ));
    let symbols = placed.iter().filter(|p| p.sym.is_some()).count();
    for (i, line) in [
        format!("{} parts · {} nets", placed.len(), circuit.nets().len()),
        format!("{symbols} drawn from KiCad symbols"),
        "legion-of-bom · schematic view".to_string(),
    ]
    .iter()
    .enumerate()
    {
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"ui-monospace,monospace\" \
             font-size=\"9\" fill=\"#6b7280\">{}</text>",
            tx + 10.0,
            ty + 42.0 + i as f64 * 10.0,
            xml_escape(line)
        ));
    }
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

    // Pin leads: from the connection point in to where the pin meets the body.
    for pin in &g.pins {
        let (rx, ry) = pt(pin.x, pin.y);
        let (bx, by) = pin.body_end();
        let (tx, ty) = pt(bx, by);
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
