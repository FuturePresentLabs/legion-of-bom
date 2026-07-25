//! Gerber (RS-274X) + Excellon reading, and an SVG view of a fab package (02b).
//!
//! Enough of the format to *see* a board the way a fab house does: apertures,
//! draws, flashes, regions, arcs, and per-layer polarity. Unlike the CAD formats
//! we might import from, RS-274X has a public specification, so this is ordinary
//! parsing rather than archaeology.
//!
//! Layers are classified by filename, because the same board means the same thing
//! whatever wrote it: our KiCad exports name layers `…-F_Cu.gtl`, DipTrace writes
//! Protel extensions (`.GTL`, `.GBO`, `.GKO`), and both land on the same
//! [`LayerKind`].

use std::collections::HashMap;
use std::path::Path;

/// What a gerber file is *for*, independent of who wrote it or how they named it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayerKind {
    /// Board outline — drawn last and always, it's the frame of reference.
    Outline,
    CopperTop,
    CopperBottom,
    SilkTop,
    SilkBottom,
    MaskTop,
    MaskBottom,
    PasteTop,
    PasteBottom,
    /// Drilled holes, from the Excellon file.
    Drill,
    /// Fabrication/assembly/courtyard drawings — informational.
    Other,
}

impl LayerKind {
    /// A short label for the viewer's layer list.
    pub fn label(self) -> &'static str {
        match self {
            LayerKind::Outline => "Outline",
            LayerKind::CopperTop => "Copper top",
            LayerKind::CopperBottom => "Copper bottom",
            LayerKind::SilkTop => "Silk top",
            LayerKind::SilkBottom => "Silk bottom",
            LayerKind::MaskTop => "Mask top",
            LayerKind::MaskBottom => "Mask bottom",
            LayerKind::PasteTop => "Paste top",
            LayerKind::PasteBottom => "Paste bottom",
            LayerKind::Drill => "Drill",
            LayerKind::Other => "Other",
        }
    }

    /// Stable key used in URLs and the UI.
    pub fn key(self) -> &'static str {
        match self {
            LayerKind::Outline => "outline",
            LayerKind::CopperTop => "cu-top",
            LayerKind::CopperBottom => "cu-bot",
            LayerKind::SilkTop => "silk-top",
            LayerKind::SilkBottom => "silk-bot",
            LayerKind::MaskTop => "mask-top",
            LayerKind::MaskBottom => "mask-bot",
            LayerKind::PasteTop => "paste-top",
            LayerKind::PasteBottom => "paste-bot",
            LayerKind::Drill => "drill",
            LayerKind::Other => "other",
        }
    }

    /// Render colour — the usual fab-viewer convention.
    fn colour(self) -> &'static str {
        match self {
            LayerKind::Outline => "#f2c14e",
            LayerKind::CopperTop => "#c8791f",
            LayerKind::CopperBottom => "#3f7fbf",
            LayerKind::SilkTop | LayerKind::SilkBottom => "#e8e8e4",
            LayerKind::MaskTop | LayerKind::MaskBottom => "#2f7d4f",
            LayerKind::PasteTop | LayerKind::PasteBottom => "#9aa0a6",
            LayerKind::Drill => "#12141a",
            LayerKind::Other => "#7a7f87",
        }
    }

    /// Draw order — outline on top, informational layers underneath.
    fn z(self) -> u8 {
        match self {
            LayerKind::MaskTop | LayerKind::MaskBottom => 0,
            LayerKind::Other => 1,
            LayerKind::CopperBottom => 2,
            LayerKind::CopperTop => 3,
            LayerKind::PasteTop | LayerKind::PasteBottom => 4,
            LayerKind::SilkTop | LayerKind::SilkBottom => 5,
            LayerKind::Drill => 6,
            LayerKind::Outline => 7,
        }
    }

    /// Classify by filename. Handles our KiCad names and Protel extensions.
    pub fn classify(file_name: &str) -> LayerKind {
        let n = file_name.to_ascii_lowercase();
        let ext = n.rsplit('.').next().unwrap_or("");
        // Protel-style extensions first — they're unambiguous.
        match ext {
            "gtl" => return LayerKind::CopperTop,
            "gbl" => return LayerKind::CopperBottom,
            "gto" => return LayerKind::SilkTop,
            "gbo" => return LayerKind::SilkBottom,
            "gts" => return LayerKind::MaskTop,
            "gbs" => return LayerKind::MaskBottom,
            "gtp" => return LayerKind::PasteTop,
            "gbp" => return LayerKind::PasteBottom,
            "gko" | "gm1" => return LayerKind::Outline,
            "drl" | "xln" | "txt" => return LayerKind::Drill,
            _ => {}
        }
        // Otherwise read the descriptive name. Tools disagree wildly here: KiCad
        // writes `-F_Cu`/`-B_Silkscreen`, DipTrace writes `1 - Top`/`TopSilk`/
        // `BoardOutline`. Match on meaning, and test the compound names first —
        // "TopSilk" is silk, not copper, and "BottomAssembly" is a drawing.
        let has = |k: &str| n.contains(k);
        let top = has("top") || has("f_cu") || has("front") || n.starts_with("1 ");
        let bottom = has("bottom") || has("b_cu") || has("back") || n.starts_with("2 ");
        let side = |t: LayerKind, b: LayerKind| if bottom && !top { b } else { t };

        if has("silk") {
            side(LayerKind::SilkTop, LayerKind::SilkBottom)
        } else if has("mask") {
            side(LayerKind::MaskTop, LayerKind::MaskBottom)
        } else if has("paste") {
            side(LayerKind::PasteTop, LayerKind::PasteBottom)
        } else if has("assembly")
            || has("dimension")
            || has("courtyard")
            || has("fab")
            || has("margin")
            || has("comment")
        {
            // Drawings for people, not for the board.
            LayerKind::Other
        } else if has("outline") || has("edge") || has("profile") || has("keepout") {
            LayerKind::Outline
        } else if top || bottom {
            side(LayerKind::CopperTop, LayerKind::CopperBottom)
        } else {
            LayerKind::Other
        }
    }
}

/// A drawable produced by the reader, in millimetres with Y up (gerber's own
/// convention); the renderer flips Y.
#[derive(Debug, Clone, PartialEq)]
pub enum Prim {
    /// A drawn track: stroked with the current aperture's diameter, round caps.
    Track {
        pts: Vec<(f64, f64)>,
        width: f64,
        clear: bool,
    },
    /// A filled shape: a flash, a region, or a macro primitive.
    Fill { pts: Vec<(f64, f64)>, clear: bool },
    /// A flashed circular pad or a drilled hole.
    Disc {
        cx: f64,
        cy: f64,
        d: f64,
        clear: bool,
    },
}

/// One parsed layer.
#[derive(Debug, Clone)]
pub struct Layer {
    pub kind: LayerKind,
    pub file_name: String,
    pub prims: Vec<Prim>,
}

/// An aperture: the tool a draw or flash is made with.
#[derive(Debug, Clone)]
enum Aperture {
    Circle(f64),
    Rect(f64, f64),
    Obround(f64, f64),
    Poly(f64, usize),
    /// An aperture macro, already expanded to outlines/discs at flash time.
    Macro(String, Vec<f64>),
}

impl Aperture {
    /// Stroke width when drawing (not flashing) with this aperture.
    fn width(&self) -> f64 {
        match self {
            Aperture::Circle(d) => *d,
            Aperture::Rect(w, h) | Aperture::Obround(w, h) => w.min(*h),
            Aperture::Poly(d, _) => *d,
            Aperture::Macro(_, args) => args.first().copied().unwrap_or(0.1),
        }
    }
}

/// Coordinate format from `%FS…%`.
#[derive(Debug, Clone, Copy)]
struct Format {
    dec: u32,
    mm: bool,
}

impl Default for Format {
    fn default() -> Self {
        Format { dec: 6, mm: true }
    }
}

impl Format {
    fn scale(&self, raw: f64) -> f64 {
        let v = raw / 10f64.powi(self.dec as i32);
        if self.mm {
            v
        } else {
            v * 25.4
        }
    }
}

/// Parse one gerber file into primitives.
pub fn parse_gerber(text: &str, file_name: &str) -> Layer {
    let mut fmt = Format::default();
    let mut apertures: HashMap<u32, Aperture> = HashMap::new();
    let mut macros: HashMap<String, Vec<String>> = HashMap::new();
    let mut cur: Option<u32> = None;
    let mut prims: Vec<Prim> = Vec::new();
    let mut pos = (0.0f64, 0.0f64);
    let mut clear = false;
    let mut region = false;
    let mut region_pts: Vec<(f64, f64)> = Vec::new();
    let mut track: Vec<(f64, f64)> = Vec::new();
    let mut arc_cw: Option<bool> = None;

    // Extended commands arrive between % … %; ordinary ones end with *.
    let mut rest = text;
    let mut pending_macro: Option<(String, Vec<String>)> = None;
    while !rest.is_empty() {
        let (chunk, tail) = match rest.find(['*', '%']) {
            Some(i) => {
                let c = &rest[..i];
                let sep = rest.as_bytes()[i];
                (c.trim(), (&rest[i + 1..], sep))
            }
            None => break,
        };
        let (next, sep) = tail;
        rest = next;
        if chunk.is_empty() {
            if sep == b'%' {
                if let Some((name, body)) = pending_macro.take() {
                    macros.insert(name, body);
                }
            }
            continue;
        }

        // Inside an aperture-macro definition, collect its primitive lines.
        if let Some((_, body)) = pending_macro.as_mut() {
            if !chunk.starts_with("%") {
                body.push(chunk.to_string());
                continue;
            }
        }

        let c = chunk.strip_prefix('%').unwrap_or(chunk);
        if let Some(spec) = c.strip_prefix("FS") {
            // e.g. LAX46Y46 — the two digits after X are int/decimal counts.
            if let Some(x) = spec.find('X') {
                let d = spec[x + 1..].chars().nth(1).and_then(|c| c.to_digit(10));
                fmt.dec = d.unwrap_or(6);
            }
        } else if let Some(u) = c.strip_prefix("MO") {
            fmt.mm = !u.starts_with("IN");
        } else if let Some(p) = c.strip_prefix("LP") {
            clear = p.starts_with('C');
        } else if let Some(m) = c.strip_prefix("AM") {
            pending_macro = Some((m.to_string(), Vec::new()));
        } else if let Some(def) = c.strip_prefix("ADD") {
            let num: u32 = def
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            let body = &def[num.to_string().len()..];
            apertures.insert(num, parse_aperture(body));
        } else if c.starts_with("TF") || c.starts_with("TA") || c.starts_with("TO") || c == "TD" {
            // Attributes — metadata, nothing to draw.
        } else {
            // An ordinary command word: G/D/M codes and coordinates, possibly
            // several concatenated (`G01X..Y..D01`).
            let mut d_code: Option<u32> = None;
            let mut x = None;
            let mut y = None;
            let mut i = None;
            let mut j = None;
            let mut it = chunk.char_indices().peekable();
            while let Some((idx, ch)) = it.next() {
                if !ch.is_ascii_alphabetic() {
                    continue;
                }
                let start = idx + 1;
                let mut end = start;
                for (k, c2) in chunk[start..].char_indices() {
                    if c2.is_ascii_digit() || c2 == '-' || c2 == '+' {
                        end = start + k + c2.len_utf8();
                    } else {
                        break;
                    }
                }
                let num: f64 = chunk[start..end].parse().unwrap_or(0.0);
                match ch {
                    'G' => match num as u32 {
                        1 => arc_cw = None,
                        2 => arc_cw = Some(true),
                        3 => arc_cw = Some(false),
                        36 => {
                            region = true;
                            region_pts.clear();
                        }
                        37 => {
                            region = false;
                            if region_pts.len() > 2 {
                                prims.push(Prim::Fill {
                                    pts: std::mem::take(&mut region_pts),
                                    clear,
                                });
                            }
                            region_pts.clear();
                        }
                        _ => {}
                    },
                    'D' => {
                        let n = num as u32;
                        if n >= 10 {
                            flush_track(&mut track, &mut prims, &apertures, cur, clear);
                            cur = Some(n);
                        } else {
                            d_code = Some(n);
                        }
                    }
                    'X' => x = Some(fmt.scale(num)),
                    'Y' => y = Some(fmt.scale(num)),
                    'I' => i = Some(fmt.scale(num)),
                    'J' => j = Some(fmt.scale(num)),
                    'M' => {}
                    _ => {}
                }
                while it.peek().is_some_and(|(k, _)| *k < end) {
                    it.next();
                }
            }

            let to = (x.unwrap_or(pos.0), y.unwrap_or(pos.1));
            match d_code {
                Some(1) => {
                    // Draw. An arc is flattened into the same point list.
                    let seg = match (arc_cw, i, j) {
                        (Some(cw), Some(i), Some(j)) => arc_points(pos, to, (i, j), cw),
                        _ => vec![to],
                    };
                    if region {
                        if region_pts.is_empty() {
                            region_pts.push(pos);
                        }
                        region_pts.extend(seg);
                    } else {
                        if track.is_empty() {
                            track.push(pos);
                        }
                        track.extend(seg);
                    }
                    pos = to;
                }
                Some(2) => {
                    // Move: ends any run of draws.
                    flush_track(&mut track, &mut prims, &apertures, cur, clear);
                    if region && !region_pts.is_empty() {
                        prims.push(Prim::Fill {
                            pts: std::mem::take(&mut region_pts),
                            clear,
                        });
                    }
                    pos = to;
                }
                Some(3) => {
                    // Flash the current aperture here.
                    if let Some(ap) = cur.and_then(|n| apertures.get(&n)) {
                        flash(ap, to, clear, &macros, &mut prims);
                    }
                    pos = to;
                }
                _ => {
                    if x.is_some() || y.is_some() {
                        pos = to;
                    }
                }
            }
        }
    }
    flush_track(&mut track, &mut prims, &apertures, cur, clear);

    Layer {
        kind: LayerKind::classify(file_name),
        file_name: file_name.to_string(),
        prims,
    }
}

fn flush_track(
    track: &mut Vec<(f64, f64)>,
    prims: &mut Vec<Prim>,
    apertures: &HashMap<u32, Aperture>,
    cur: Option<u32>,
    clear: bool,
) {
    if track.len() > 1 {
        let width = cur
            .and_then(|n| apertures.get(&n))
            .map(|a| a.width())
            .unwrap_or(0.15);
        prims.push(Prim::Track {
            pts: std::mem::take(track),
            width,
            clear,
        });
    } else {
        track.clear();
    }
}

/// `C,0.15` / `R,1.7X1.7` / `O,1.6X2.0` / `P,2X6` / `MyMacro,1X2X3`.
fn parse_aperture(body: &str) -> Aperture {
    let (shape, args) = body.split_once(',').unwrap_or((body, ""));
    let nums: Vec<f64> = args
        .split('X')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect();
    let g = |i: usize| nums.get(i).copied().unwrap_or(0.0);
    match shape.trim() {
        "C" => Aperture::Circle(g(0)),
        "R" => Aperture::Rect(g(0), g(1)),
        "O" => Aperture::Obround(g(0), g(1)),
        "P" => Aperture::Poly(g(0), g(1) as usize),
        name => Aperture::Macro(name.to_string(), nums),
    }
}

/// Turn a flash into fills. Macros are expanded from their stored body.
fn flash(
    ap: &Aperture,
    at: (f64, f64),
    clear: bool,
    macros: &HashMap<String, Vec<String>>,
    out: &mut Vec<Prim>,
) {
    let (cx, cy) = at;
    match ap {
        Aperture::Circle(d) => out.push(Prim::Disc {
            cx,
            cy,
            d: *d,
            clear,
        }),
        Aperture::Rect(w, h) => out.push(Prim::Fill {
            pts: rect_pts(cx, cy, *w, *h),
            clear,
        }),
        Aperture::Obround(w, h) => {
            // A stadium: a track as wide as the short axis, run along the long one.
            let (w, h) = (*w, *h);
            let r = w.min(h);
            let run = (w.max(h) - r) / 2.0;
            let pts = if w >= h {
                vec![(cx - run, cy), (cx + run, cy)]
            } else {
                vec![(cx, cy - run), (cx, cy + run)]
            };
            out.push(Prim::Track {
                pts,
                width: r,
                clear,
            });
        }
        Aperture::Poly(d, n) => {
            let n = (*n).max(3);
            let r = d / 2.0;
            let pts = (0..n)
                .map(|k| {
                    let a = std::f64::consts::TAU * k as f64 / n as f64;
                    (cx + r * a.cos(), cy + r * a.sin())
                })
                .collect();
            out.push(Prim::Fill { pts, clear });
        }
        Aperture::Macro(name, args) => {
            let Some(body) = macros.get(name) else {
                // Unknown macro: a disc of roughly the right size still shows the
                // pad, which beats dropping it silently.
                out.push(Prim::Disc {
                    cx,
                    cy,
                    d: args.first().copied().unwrap_or(1.0).max(0.2),
                    clear,
                });
                return;
            };
            expand_macro(body, args, at, clear, out);
        }
    }
}

/// Expand an aperture macro's primitives. Supports the shapes KiCad actually
/// emits: 1 (circle), 4 (outline), 20 (vector line), 21 (centre line).
fn expand_macro(body: &[String], args: &[f64], at: (f64, f64), clear: bool, out: &mut Vec<Prim>) {
    let (cx, cy) = at;
    for line in body {
        let line = line.trim();
        if line.starts_with('0') || line.is_empty() {
            continue; // a comment primitive
        }
        let f: Vec<f64> = line.split(',').map(|t| eval(t.trim(), args)).collect();
        let g = |i: usize| f.get(i).copied().unwrap_or(0.0);
        match f.first().map(|v| *v as i32) {
            Some(1) => out.push(Prim::Disc {
                cx: cx + g(3),
                cy: cy + g(4),
                d: g(2),
                clear: clear || g(1) == 0.0,
            }),
            Some(4) => {
                // exposure, count, then x/y pairs.
                let n = g(2) as usize;
                let mut pts = Vec::with_capacity(n + 1);
                for k in 0..=n {
                    pts.push((cx + g(3 + k * 2), cy + g(4 + k * 2)));
                }
                if pts.len() > 2 {
                    out.push(Prim::Fill {
                        pts,
                        clear: clear || g(1) == 0.0,
                    });
                }
            }
            Some(20) => out.push(Prim::Track {
                pts: vec![(cx + g(3), cy + g(4)), (cx + g(5), cy + g(6))],
                width: g(2),
                clear: clear || g(1) == 0.0,
            }),
            Some(21) => out.push(Prim::Fill {
                pts: rect_pts(cx + g(4), cy + g(5), g(2), g(3)),
                clear: clear || g(1) == 0.0,
            }),
            _ => {}
        }
    }
}

/// Evaluate a macro expression: numbers, `$n` parameters, and + - x /.
fn eval(expr: &str, args: &[f64]) -> f64 {
    // Split on the lowest-precedence operators first, right to left.
    let bytes = expr.as_bytes();
    for (ops, _) in [(['+', '-'], 0), (['x', '/'], 1)] {
        let mut depth = 0i32;
        for i in (0..bytes.len()).rev() {
            match bytes[i] {
                b')' => depth += 1,
                b'(' => depth -= 1,
                _ if depth == 0 && i > 0 && ops.contains(&(bytes[i] as char)) => {
                    let (l, r) = expr.split_at(i);
                    let rv = eval(&r[1..], args);
                    let lv = eval(l, args);
                    return match bytes[i] {
                        b'+' => lv + rv,
                        b'-' => lv - rv,
                        b'x' => lv * rv,
                        _ => {
                            if rv == 0.0 {
                                0.0
                            } else {
                                lv / rv
                            }
                        }
                    };
                }
                _ => {}
            }
        }
    }
    let e = expr.trim().trim_start_matches('(').trim_end_matches(')');
    if let Some(n) = e.strip_prefix('$') {
        let idx: usize = n.parse().unwrap_or(0);
        return args.get(idx.saturating_sub(1)).copied().unwrap_or(0.0);
    }
    e.parse().unwrap_or(0.0)
}

fn rect_pts(cx: f64, cy: f64, w: f64, h: f64) -> Vec<(f64, f64)> {
    let (hw, hh) = (w / 2.0, h / 2.0);
    vec![
        (cx - hw, cy - hh),
        (cx + hw, cy - hh),
        (cx + hw, cy + hh),
        (cx - hw, cy + hh),
    ]
}

/// Flatten an arc into points. `ij` is the centre offset from the start.
fn arc_points(from: (f64, f64), to: (f64, f64), ij: (f64, f64), cw: bool) -> Vec<(f64, f64)> {
    let c = (from.0 + ij.0, from.1 + ij.1);
    let r = (from.0 - c.0).hypot(from.1 - c.1);
    if r < 1e-9 {
        return vec![to];
    }
    let a0 = (from.1 - c.1).atan2(from.0 - c.0);
    let a1 = (to.1 - c.1).atan2(to.0 - c.0);
    let mut sweep = a1 - a0;
    if cw && sweep > 0.0 {
        sweep -= std::f64::consts::TAU;
    }
    if !cw && sweep < 0.0 {
        sweep += std::f64::consts::TAU;
    }
    // A full circle shows up as a zero sweep between identical endpoints.
    if sweep.abs() < 1e-12 {
        sweep = if cw {
            -std::f64::consts::TAU
        } else {
            std::f64::consts::TAU
        };
    }
    let steps = ((sweep.abs() * r / 0.05).ceil() as usize).clamp(4, 256);
    (1..=steps)
        .map(|k| {
            let a = a0 + sweep * k as f64 / steps as f64;
            (c.0 + r * a.cos(), c.1 + r * a.sin())
        })
        .collect()
}

/// Parse an Excellon drill file into holes.
pub fn parse_drill(text: &str, file_name: &str) -> Layer {
    let mut tools: HashMap<u32, f64> = HashMap::new();
    let mut cur = 0u32;
    let mut metric = true;
    let mut prims = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("METRIC") {
            metric = true;
        } else if l.starts_with("INCH") {
            metric = false;
        } else if let Some(rest) = l.strip_prefix('T') {
            let num: u32 = rest
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            if let Some(ci) = l.find('C') {
                let d: f64 = l[ci + 1..]
                    .trim()
                    .trim_start_matches('+')
                    .parse()
                    .unwrap_or(0.0);
                tools.insert(num, if metric { d } else { d * 25.4 });
            } else {
                cur = num;
            }
        } else if l.starts_with('X') || l.starts_with('Y') {
            // Coordinates may be signed, and are often written with an *implicit*
            // decimal point — `X+013476` is 1.3476 inches, not 13476 of anything.
            // Reading those literally puts every hole thousands of mm off-board,
            // which is silent: the holes simply never appear.
            let get = |k: char| -> Option<f64> {
                let i = l.find(k)?;
                let s = &l[i + 1..];
                let end = s
                    .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
                    .unwrap_or(s.len());
                let tok = &s[..end];
                let v: f64 = tok.trim_start_matches('+').parse().ok()?;
                Some(if tok.contains('.') {
                    v
                } else {
                    // Excellon's usual implicit formats: 2.4 for inch, 3.3 for mm.
                    v / if metric { 1_000.0 } else { 10_000.0 }
                })
            };
            if let (Some(x), Some(y)) = (get('X'), get('Y')) {
                let s = if metric { 1.0 } else { 25.4 };
                prims.push(Prim::Disc {
                    cx: x * s,
                    cy: y * s,
                    d: tools.get(&cur).copied().unwrap_or(0.3),
                    clear: false,
                });
            }
        }
    }
    Layer {
        kind: LayerKind::Drill,
        file_name: file_name.to_string(),
        prims,
    }
}

/// Read every gerber/drill file in a directory.
pub fn read_layers(dir: &Path) -> std::io::Result<Vec<Layer>> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".gbrjob") || !path.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let kind = LayerKind::classify(name);
        out.push(if kind == LayerKind::Drill {
            parse_drill(&text, name)
        } else {
            parse_gerber(&text, name)
        });
    }
    Ok(out)
}

/// Bounding box over every layer, in mm.
fn bounds(layers: &[Layer]) -> (f64, f64, f64, f64) {
    let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut add = |x: f64, y: f64, r: f64| {
        b.0 = b.0.min(x - r);
        b.1 = b.1.min(y - r);
        b.2 = b.2.max(x + r);
        b.3 = b.3.max(y + r);
    };
    for l in layers {
        for p in &l.prims {
            match p {
                Prim::Track { pts, width, .. } => {
                    for &(x, y) in pts {
                        add(x, y, width / 2.0);
                    }
                }
                Prim::Fill { pts, .. } => {
                    for &(x, y) in pts {
                        add(x, y, 0.0);
                    }
                }
                Prim::Disc { cx, cy, d, .. } => add(*cx, *cy, d / 2.0),
            }
        }
    }
    if b.0 > b.2 {
        (0.0, 0.0, 10.0, 10.0)
    } else {
        b
    }
}

/// Render layers to a standalone SVG. `show` selects which kinds are drawn; an
/// empty selection means all of them.
pub fn layers_to_svg(layers: &[Layer], show: &[LayerKind]) -> String {
    let (x0, y0, x1, y1) = bounds(layers);
    let pad = 2.0;
    let (w, h) = (x1 - x0 + 2.0 * pad, y1 - y0 + 2.0 * pad);
    // Gerber Y points up, SVG down: flip about the board's own extent.
    let fy = |y: f64| y1 + pad - y;
    let fx = |x: f64| x - x0 + pad;

    let mut visible: Vec<&Layer> = layers
        .iter()
        .filter(|l| show.is_empty() || show.contains(&l.kind))
        .collect();
    visible.sort_by_key(|l| l.kind.z());

    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w:.3} {h:.3}\" \
         width=\"{:.0}\" height=\"{:.0}\" role=\"img\" aria-label=\"gerber layers\">\
         <rect width=\"{w:.3}\" height=\"{h:.3}\" fill=\"#0f1116\"/>",
        w * 8.0,
        h * 8.0
    );
    for l in visible {
        let col = l.kind.colour();
        s.push_str(&format!(
            "<g id=\"layer-{}\" fill=\"{col}\" stroke=\"{col}\" fill-rule=\"evenodd\">",
            l.kind.key()
        ));
        for p in &l.prims {
            // A clear (negative) primitive paints the board colour back over what
            // is under it — how a gerber carves a clearance out of a pour.
            let (fill, stroke) = match p {
                Prim::Track { clear, .. } | Prim::Fill { clear, .. } | Prim::Disc { clear, .. } => {
                    if *clear {
                        ("#0f1116", "#0f1116")
                    } else {
                        (col, col)
                    }
                }
            };
            match p {
                Prim::Track { pts, width, .. } => {
                    let d: Vec<String> = pts
                        .iter()
                        .enumerate()
                        .map(|(i, &(x, y))| {
                            format!(
                                "{}{:.3} {:.3}",
                                if i == 0 { 'M' } else { 'L' },
                                fx(x),
                                fy(y)
                            )
                        })
                        .collect();
                    s.push_str(&format!(
                        "<path d=\"{}\" fill=\"none\" stroke=\"{stroke}\" \
                         stroke-width=\"{width:.3}\" stroke-linecap=\"round\" \
                         stroke-linejoin=\"round\"/>",
                        d.join(" ")
                    ));
                }
                Prim::Fill { pts, .. } => {
                    let d: Vec<String> = pts
                        .iter()
                        .map(|&(x, y)| format!("{:.3},{:.3}", fx(x), fy(y)))
                        .collect();
                    s.push_str(&format!(
                        "<polygon points=\"{}\" fill=\"{fill}\" stroke=\"none\"/>",
                        d.join(" ")
                    ));
                }
                Prim::Disc { cx, cy, d, .. } => {
                    s.push_str(&format!(
                        "<circle cx=\"{:.3}\" cy=\"{:.3}\" r=\"{:.3}\" fill=\"{fill}\" \
                         stroke=\"none\"/>",
                        fx(*cx),
                        fy(*cy),
                        d / 2.0
                    ));
                }
            }
        }
        s.push_str("</g>");
    }
    s.push_str("</svg>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DipTrace writes descriptive names with a plain `.gbr` extension, so the
    /// classifier has to read meaning rather than lean on the extension — and it
    /// has to test the compound names first, or `TopSilk` reads as copper.
    #[test]
    fn classifies_diptrace_descriptive_names() {
        for (name, want) in [
            ("1 - Top.gbr", LayerKind::CopperTop),
            ("2 - Bottom.gbr", LayerKind::CopperBottom),
            ("TopSilk.gbr", LayerKind::SilkTop),
            ("BottomSilk.gbr", LayerKind::SilkBottom),
            ("TopMask.gbr", LayerKind::MaskTop),
            ("BottomPaste.gbr", LayerKind::PasteBottom),
            ("BoardOutline.gbr", LayerKind::Outline),
            ("BottomAssembly.gbr", LayerKind::Other),
            ("TopDimension.gbr", LayerKind::Other),
            ("Through.drl", LayerKind::Drill),
        ] {
            assert_eq!(LayerKind::classify(name), want, "{name}");
        }
    }

    #[test]
    fn classifies_kicad_and_protel_names() {
        assert_eq!(
            LayerKind::classify("slew_limiter-F_Cu.gtl"),
            LayerKind::CopperTop
        );
        assert_eq!(LayerKind::classify("PHRSR_V7.GBL"), LayerKind::CopperBottom);
        assert_eq!(
            LayerKind::classify("board-Edge_Cuts.gm1"),
            LayerKind::Outline
        );
        assert_eq!(LayerKind::classify("PHRSR.GKO"), LayerKind::Outline);
        assert_eq!(LayerKind::classify("board.drl"), LayerKind::Drill);
        assert_eq!(
            LayerKind::classify("board-B_Silkscreen.gbo"),
            LayerKind::SilkBottom
        );
    }

    /// The outline of a real KiCad Edge.Cuts export: format, aperture, then a
    /// move and three draws closing the rectangle.
    #[test]
    fn reads_a_rectangular_outline() {
        let g = "%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,0.150000*%\nD10*\n\
                 X0Y128500000D02*\nX25400000Y128500000D01*\nX25400000Y0D01*\n\
                 X0Y0D01*\nX0Y128500000D01*\nM02*\n";
        let l = parse_gerber(g, "b-Edge_Cuts.gm1");
        assert_eq!(l.kind, LayerKind::Outline);
        let Some(Prim::Track { pts, width, .. }) = l.prims.first() else {
            panic!("expected a track, got {:?}", l.prims);
        };
        assert!((width - 0.15).abs() < 1e-9);
        // 25.4 mm x 128.5 mm — a 5 HP Eurorack board.
        let xs: Vec<f64> = pts.iter().map(|p| p.0).collect();
        let ys: Vec<f64> = pts.iter().map(|p| p.1).collect();
        let span = |v: &[f64]| v.iter().cloned().fold(f64::MIN, f64::max);
        assert!((span(&xs) - 25.4).abs() < 1e-6, "{xs:?}");
        assert!((span(&ys) - 128.5).abs() < 1e-6, "{ys:?}");
    }

    #[test]
    fn flashes_pads_and_fills_regions() {
        let g = "%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,1.0*%\n%ADD11R,2.0X1.0*%\n\
                 D10*\nX1000000Y1000000D03*\n\
                 D11*\nX5000000Y1000000D03*\n\
                 G36*\nX0Y0D02*\nG01*\nX2000000Y0D01*\nX2000000Y2000000D01*\nG37*\nM02*\n";
        let l = parse_gerber(g, "x-F_Cu.gtl");
        let discs = l
            .prims
            .iter()
            .filter(|p| matches!(p, Prim::Disc { .. }))
            .count();
        let fills = l
            .prims
            .iter()
            .filter(|p| matches!(p, Prim::Fill { .. }))
            .count();
        assert_eq!(discs, 1, "circle flash");
        assert!(fills >= 2, "rect flash + region: {:?}", l.prims);
    }

    /// Inches must be converted; a 0.1" step is 2.54 mm.
    #[test]
    fn honours_imperial_units() {
        let g = "%FSLAX24Y24*%\n%MOIN*%\n%ADD10C,0.010*%\nD10*\n\
                 X0Y0D02*\nX1000Y0D01*\nM02*\n";
        let l = parse_gerber(g, "x.gtl");
        let Some(Prim::Track { pts, .. }) = l.prims.first() else {
            panic!("expected a track");
        };
        assert!((pts[1].0 - 2.54).abs() < 1e-6, "{pts:?}");
    }

    #[test]
    fn macro_expressions_resolve_parameters() {
        assert!((eval("$1+$1", &[0.25]) - 0.5).abs() < 1e-9);
        assert!((eval("2x$2", &[0.0, 3.0]) - 6.0).abs() < 1e-9);
        assert!((eval("1.5", &[]) - 1.5).abs() < 1e-9);
    }

    /// DipTrace writes signed, implicit-decimal Excellon: `X+013476` is 1.3476
    /// inches. Parsed literally the hole lands thousands of mm away and silently
    /// vanishes from the board — which is exactly what happened.
    #[test]
    fn reads_implicit_decimal_inch_drill() {
        let d = "M48\nINCH\nT01C0.0394\n%\nT01\nX+013476Y+044146\nM30\n";
        let l = parse_drill(d, "Through.drl");
        assert_eq!(l.prims.len(), 1, "the hole must not be dropped");
        let Some(Prim::Disc { cx, cy, d, .. }) = l.prims.first() else {
            panic!("expected a hole");
        };
        // 1.3476in = 34.229mm, 4.4146in = 112.13mm, tool 0.0394in = 1.0mm.
        assert!((cx - 34.229).abs() < 0.01, "x={cx}");
        assert!((cy - 112.13).abs() < 0.01, "y={cy}");
        assert!((d - 1.0).abs() < 0.01, "d={d}");
    }

    #[test]
    fn reads_excellon_holes() {
        let d = "METRIC\nT1C0.800\nT1\nX10.0Y20.0\nX11.0Y20.0\nM30\n";
        let l = parse_drill(d, "b.drl");
        assert_eq!(l.kind, LayerKind::Drill);
        assert_eq!(l.prims.len(), 2);
        let Some(Prim::Disc { d, cx, .. }) = l.prims.first() else {
            panic!("expected holes");
        };
        assert!((d - 0.8).abs() < 1e-9 && (cx - 10.0).abs() < 1e-9);
    }
}
