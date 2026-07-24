//! SVG logo → placed silk polygons (DESIGN §7.9's "logo as a scalable vector").
//!
//! A small, self-contained flattener for the path subset our brand assets use —
//! `M/L/C/Z` (absolute and relative), which is what Inkscape emits for a filled
//! mark. Curves are flattened to polylines so the same geometry drives PCB silk
//! (`gr_poly`), panel silk, and — later — a laser/DXF engrave layer. Deliberately
//! not a general SVG renderer: no arcs, gradients, or text (a logo is authored
//! once as outlined vector art, not composed here).

/// Points sampled along each cubic Bézier segment. Plenty for a ~40 mm silk mark.
const BEZIER_STEPS: usize = 24;

/// A logo: closed subpaths (outlines + counters), in the SVG's user units.
#[derive(Debug, Clone, PartialEq)]
pub struct Logo {
    /// Each subpath is a closed polyline of `(x, y)` in SVG units (y-down, as SVG
    /// and KiCad both are).
    pub subpaths: Vec<Vec<(f64, f64)>>,
}

impl Logo {
    /// Parse the (longest) `<path d="…">` out of an SVG document and flatten it.
    pub fn from_svg(svg: &str) -> Result<Logo, String> {
        let d = longest_path_d(svg).ok_or("no <path d=\"…\"> found in the SVG")?;
        Logo::from_path_d(&d)
    }

    /// Flatten an SVG path `d` string (M/L/C/Z, absolute + relative) to polylines.
    pub fn from_path_d(d: &str) -> Result<Logo, String> {
        let toks = tokenize(d);
        let mut subpaths: Vec<Vec<(f64, f64)>> = Vec::new();
        let mut cur: Vec<(f64, f64)> = Vec::new();
        let (mut x, mut y) = (0.0f64, 0.0f64); // current point
        let (mut sx, mut sy) = (0.0f64, 0.0f64); // subpath start (for Z)
        let mut i = 0;
        let mut cmd = ' ';
        // Read the next `n` numbers; error if the path runs out mid-command.
        let take = |i: &mut usize, n: usize| -> Result<Vec<f64>, String> {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                match toks.get(*i) {
                    Some(Tok::Num(f)) => {
                        v.push(*f);
                        *i += 1;
                    }
                    _ => return Err("path command missing coordinates".into()),
                }
            }
            Ok(v)
        };
        while i < toks.len() {
            match toks[i] {
                Tok::Cmd(c) => {
                    cmd = c;
                    i += 1;
                }
                Tok::Num(_) => { /* repeat the previous command with fresh coords */ }
            }
            let rel = cmd.is_ascii_lowercase();
            match cmd.to_ascii_uppercase() {
                'M' => {
                    let p = take(&mut i, 2)?;
                    if !cur.is_empty() {
                        subpaths.push(std::mem::take(&mut cur));
                    }
                    (x, y) = if rel {
                        (x + p[0], y + p[1])
                    } else {
                        (p[0], p[1])
                    };
                    (sx, sy) = (x, y);
                    cur.push((x, y));
                    // A subsequent coordinate pair after M is an implicit L.
                    cmd = if rel { 'l' } else { 'L' };
                }
                'L' => {
                    let p = take(&mut i, 2)?;
                    (x, y) = if rel {
                        (x + p[0], y + p[1])
                    } else {
                        (p[0], p[1])
                    };
                    cur.push((x, y));
                }
                'C' => {
                    let p = take(&mut i, 6)?;
                    let (x1, y1, x2, y2, ex, ey) = if rel {
                        (x + p[0], y + p[1], x + p[2], y + p[3], x + p[4], y + p[5])
                    } else {
                        (p[0], p[1], p[2], p[3], p[4], p[5])
                    };
                    flatten_cubic((x, y), (x1, y1), (x2, y2), (ex, ey), &mut cur);
                    (x, y) = (ex, ey);
                }
                'Z' => {
                    if !cur.is_empty() {
                        cur.push((sx, sy));
                        subpaths.push(std::mem::take(&mut cur));
                    }
                    (x, y) = (sx, sy);
                }
                other => return Err(format!("unsupported path command '{other}'")),
            }
        }
        if !cur.is_empty() {
            subpaths.push(cur);
        }
        if subpaths.is_empty() {
            return Err("path produced no geometry".into());
        }
        Ok(Logo { subpaths })
    }

    /// Bounding box `(min_x, min_y, max_x, max_y)` over all subpaths.
    pub fn bbox(&self) -> (f64, f64, f64, f64) {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for sp in &self.subpaths {
            for &(px, py) in sp {
                x0 = x0.min(px);
                y0 = y0.min(py);
                x1 = x1.max(px);
                y1 = y1.max(py);
            }
        }
        (x0, y0, x1, y1)
    }

    /// Place the logo in board/KiCad coordinates: uniformly scaled so its width is
    /// `target_w_mm`, centred at `center`. `mirror_x` flips it horizontally about
    /// its centre (for a back-copper silk layer, so it reads from the back).
    pub fn place(
        &self,
        target_w_mm: f64,
        center: (f64, f64),
        mirror_x: bool,
    ) -> Vec<Vec<(f64, f64)>> {
        let (x0, y0, x1, y1) = self.bbox();
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0.0 || h <= 0.0 {
            return Vec::new();
        }
        let s = target_w_mm / w;
        let (bcx, bcy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        self.subpaths
            .iter()
            .map(|sp| {
                sp.iter()
                    .map(|&(px, py)| {
                        let dx = (px - bcx) * s * if mirror_x { -1.0 } else { 1.0 };
                        let dy = (py - bcy) * s;
                        (center.0 + dx, center.1 + dy)
                    })
                    .collect()
            })
            .collect()
    }
}

/// One KiCad `gr_poly` block per placed subpath, on `layer`. `filled` fills each
/// contour (`fill solid`) — right for a mark with no enclosed counters; `false`
/// strokes the outline only (`fill none`), which renders any logo correctly
/// regardless of holes and matches a vector/laser engrave. `seed` keys the
/// deterministic uuids.
pub fn gr_polys(polys: &[Vec<(f64, f64)>], layer: &str, filled: bool, seed: &str) -> Vec<String> {
    polys
        .iter()
        .enumerate()
        .filter(|(_, sp)| sp.len() >= 2)
        .map(|(i, sp)| {
            let pts: String = sp
                .iter()
                .map(|&(x, y)| format!("(xy {} {})", crate::board::mm(x), crate::board::mm(y)))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "  (gr_poly (pts {pts}) (stroke (width 0.12) (type solid)) (fill {}) \
                 (layer \"{layer}\") (uuid \"{}\"))\n",
                if filled { "solid" } else { "none" },
                crate::board::det_uuid(&format!("{seed}.{i}")),
            )
        })
        .collect()
}

/// Flatten a cubic Bézier onto `out` (the start point is assumed already pushed).
fn flatten_cubic(
    p0: (f64, f64),
    p1: (f64, f64),
    p2: (f64, f64),
    p3: (f64, f64),
    out: &mut Vec<(f64, f64)>,
) {
    for k in 1..=BEZIER_STEPS {
        let t = k as f64 / BEZIER_STEPS as f64;
        let u = 1.0 - t;
        let b0 = u * u * u;
        let b1 = 3.0 * u * u * t;
        let b2 = 3.0 * u * t * t;
        let b3 = t * t * t;
        out.push((
            b0 * p0.0 + b1 * p1.0 + b2 * p2.0 + b3 * p3.0,
            b0 * p0.1 + b1 * p1.1 + b2 * p2.1 + b3 * p3.1,
        ));
    }
}

/// A path token: a command letter or a number.
enum Tok {
    Cmd(char),
    Num(f64),
}

/// Split a path `d` string into command/number tokens. Numbers may be separated
/// by commas, whitespace, or nothing (a leading `-`/`.` starts a new number), and
/// may carry an exponent.
fn tokenize(d: &str) -> Vec<Tok> {
    let b = d.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphabetic() {
            out.push(Tok::Cmd(c as char));
            i += 1;
        } else if c == b',' || c.is_ascii_whitespace() {
            i += 1;
        } else {
            let start = i;
            if b[i] == b'+' || b[i] == b'-' {
                i += 1;
            }
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i < b.len() && b[i] == b'.' {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                i += 1;
                if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                    i += 1;
                }
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i > start {
                if let Ok(n) = d[start..i].parse::<f64>() {
                    out.push(Tok::Num(n));
                }
            } else {
                i += 1; // skip an unexpected byte rather than loop forever
            }
        }
    }
    out
}

/// The value of the longest `d="…"` attribute — the path element, not the short
/// `id`/`inkscape:*` attributes that also happen to be named `d` on other tags.
fn longest_path_d(svg: &str) -> Option<String> {
    let mut best: Option<&str> = None;
    let mut rest = svg;
    while let Some(p) = rest.find("d=\"") {
        let after = &rest[p + 3..];
        if let Some(end) = after.find('"') {
            let val = &after[..end];
            if best.is_none_or(|b| val.len() > b.len()) {
                best = Some(val);
            }
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    best.map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_absolute_line_square() {
        let logo = Logo::from_path_d("M 0 0 L 10 0 L 10 10 L 0 10 Z").unwrap();
        assert_eq!(logo.subpaths.len(), 1);
        assert_eq!(logo.bbox(), (0.0, 0.0, 10.0, 10.0));
        // Closed: last point returns to the start.
        let sp = &logo.subpaths[0];
        assert_eq!(sp.first(), sp.last());
    }

    #[test]
    fn relative_commands_and_implicit_lineto() {
        // `m` then extra pairs are implicit relative linetos.
        let logo = Logo::from_path_d("m 0 0 10 0 0 10 -10 0 z").unwrap();
        assert_eq!(logo.bbox(), (0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn flattens_a_cubic_into_many_points() {
        let logo = Logo::from_path_d("M 0 0 C 0 10 10 10 10 0").unwrap();
        // Start point + BEZIER_STEPS samples.
        assert_eq!(logo.subpaths[0].len(), BEZIER_STEPS + 1);
        // The curve bows upward (y>0) between the endpoints.
        assert!(logo.subpaths[0].iter().any(|&(_, y)| y > 3.0));
    }

    #[test]
    fn place_scales_centres_and_mirrors() {
        let logo = Logo::from_path_d("M 0 0 L 20 0 L 20 10 L 0 10 Z").unwrap();
        let placed = logo.place(40.0, (100.0, 50.0), false);
        // Width 20 scaled to 40 → the polyline spans 40 mm centred on x=100.
        let xs: Vec<f64> = placed[0].iter().map(|p| p.0).collect();
        let (lo, hi) = (
            xs.iter().cloned().fold(f64::MAX, f64::min),
            xs.iter().cloned().fold(f64::MIN, f64::max),
        );
        assert!((hi - lo - 40.0).abs() < 1e-6);
        assert!(((lo + hi) / 2.0 - 100.0).abs() < 1e-6);
    }

    #[test]
    fn longest_d_ignores_id_attributes() {
        let svg = r#"<svg><g id="l"/><path id="p1" d="M 0 0 L 5 5 Z"/></svg>"#;
        let logo = Logo::from_svg(svg).unwrap();
        assert_eq!(logo.subpaths.len(), 1);
    }

    #[test]
    fn gr_polys_emit_one_block_per_subpath_on_layer() {
        let logo = Logo::from_path_d("M 0 0 L 10 0 L 10 10 Z M 2 2 L 4 2 L 4 4 Z").unwrap();
        let placed = logo.place(20.0, (0.0, 0.0), false);
        let outline = gr_polys(&placed, "B.SilkS", false, "t");
        assert_eq!(outline.len(), 2, "one gr_poly per subpath");
        assert!(outline[0].contains(r#"(layer "B.SilkS")"#));
        assert!(
            outline[0].contains("(fill none)"),
            "outline: {}",
            outline[0]
        );
        let filled = gr_polys(&placed, "F.SilkS", true, "t");
        assert!(filled[0].contains("(fill solid)"));
    }
}
