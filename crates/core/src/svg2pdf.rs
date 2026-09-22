//! Translates the closed SVG subset [`crate::schematic::schematic_to_svg`]
//! emits into a PDF, via [`crate::pdf`].
//!
//! Deliberately not a general SVG parser: it understands exactly the tags
//! and attributes this codebase's own SVG generator produces (`<svg>`,
//! `<rect>`, `<circle>`, `<path d="M/L/A...">`, `<polyline>`, `<text>`), and
//! fails loud on anything else, so a future SVG generator change that adds a
//! new construct is caught here rather than silently dropped from the PDF.
//!
//! PDF export is a *review* artifact, not an archival-grade one, so two
//! cosmetic simplifications are made on purpose rather than chased for
//! pixel-fidelity: a rounded rect's `rx` is drawn as a sharp corner, and
//! text width (for `text-anchor`) and vertical centering (for
//! `dominant-baseline="middle"`) use a fixed-ratio approximation instead of
//! real Helvetica glyph metrics.

use crate::pdf::{Font, Page, Paint, PathOp};

/// Average glyph width as a fraction of font size, for Helvetica — close
/// enough to right-align/center short technical labels without embedding
/// real font metrics.
const AVG_CHAR_WIDTH: f64 = 0.56;

#[derive(Debug, thiserror::Error)]
pub enum Svg2PdfError {
    #[error("no <svg> root element found")]
    MissingRoot,
    #[error("<svg> root is missing a numeric '{0}' attribute")]
    MissingDimension(&'static str),
    #[error("unrecognized top-level element '<{0}'")]
    UnsupportedElement(String),
    #[error("'{element}' is missing its '{attr}' attribute")]
    MissingAttribute {
        element: &'static str,
        attr: &'static str,
    },
    #[error("'{element}' has a malformed '{attr}' value: {value:?}")]
    MalformedAttribute {
        element: &'static str,
        attr: &'static str,
        value: String,
    },
    #[error("malformed path data: {0:?}")]
    BadPathData(String),
    #[error("unrecognized color: {0:?}")]
    BadColor(String),
}

/// Renders `svg` (the exact subset [`crate::schematic::schematic_to_svg`]
/// produces) into a single-page PDF, one SVG unit = one PDF point (the
/// diagram has no physical scale of its own, so the page simply matches the
/// content's own size).
pub fn svg_to_pdf_bytes(svg: &str) -> Result<Vec<u8>, Svg2PdfError> {
    let (page, w, h) = svg_to_page(svg)?;
    Ok(crate::pdf::document(&[page], &[], (w, h)))
}

/// As [`svg_to_pdf_bytes`], returning the built [`Page`] plus its `(width,
/// height)` in points instead of the assembled document bytes — for a
/// caller that wants to compose several pages itself.
pub fn svg_to_page(svg: &str) -> Result<(Page, f64, f64), Svg2PdfError> {
    let root_start = svg.find("<svg").ok_or(Svg2PdfError::MissingRoot)?;
    let root_end = svg[root_start..]
        .find('>')
        .map(|i| root_start + i)
        .ok_or(Svg2PdfError::MissingRoot)?;
    let root_tag = &svg[root_start..=root_end];
    let w = attr(root_tag, "width")
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or(Svg2PdfError::MissingDimension("width"))?;
    let h = attr(root_tag, "height")
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or(Svg2PdfError::MissingDimension("height"))?;

    let mut page = Page::new();
    let flip = |y: f64| h - y;

    let mut i = root_end + 1;
    let body_end = svg.rfind("</svg>").unwrap_or(svg.len());
    while i < body_end {
        let Some(lt) = svg[i..body_end].find('<') else {
            break;
        };
        let start = i + lt;
        let Some(gt) = svg[start..body_end].find('>') else {
            return Err(Svg2PdfError::BadPathData("unterminated tag".into()));
        };
        let end = start + gt;
        let tag = &svg[start..=end];
        let name = tag[1..]
            .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .next()
            .unwrap_or("");

        match name {
            "rect" => draw_rect(&mut page, tag, flip)?,
            "circle" => draw_circle(&mut page, tag, flip)?,
            "path" => draw_path(&mut page, tag, flip)?,
            "polyline" => draw_polyline(&mut page, tag, flip)?,
            "text" => {
                let close = svg[end + 1..body_end].find("</text>").ok_or(
                    Svg2PdfError::MissingAttribute {
                        element: "text",
                        attr: "(closing tag)",
                    },
                )?;
                let content = &svg[end + 1..end + 1 + close];
                draw_text(&mut page, tag, xml_unescape(content).as_str(), flip)?;
                i = end + 1 + close + "</text>".len();
                continue;
            }
            // The sheet-background rect and the SVG root itself are both
            // <rect>/<svg> and already handled generically; anything else
            // (comments, the closing </svg>) just gets skipped.
            "" | "/svg" => {}
            other => return Err(Svg2PdfError::UnsupportedElement(other.to_string())),
        }
        i = end + 1;
    }

    Ok((page, w, h))
}

fn draw_rect(page: &mut Page, tag: &str, flip: impl Fn(f64) -> f64) -> Result<(), Svg2PdfError> {
    // x/y default to 0 per the SVG spec — the sheet-background rect this
    // codebase emits omits both rather than writing "0.0" explicitly.
    let x = opt_f64(tag, "rect", "x")?.unwrap_or(0.0);
    let y = opt_f64(tag, "rect", "y")?.unwrap_or(0.0);
    let w = req_f64(tag, "rect", "width")?;
    let h = req_f64(tag, "rect", "height")?;
    let paint = paint_of(page, tag, "rect")?;
    // rx (rounded corners) is drawn sharp — see module docs.
    page.rect(x, flip(y) - h, w, h, paint);
    Ok(())
}

fn draw_circle(page: &mut Page, tag: &str, flip: impl Fn(f64) -> f64) -> Result<(), Svg2PdfError> {
    let cx = req_f64(tag, "circle", "cx")?;
    let cy = req_f64(tag, "circle", "cy")?;
    let r = req_f64(tag, "circle", "r")?;
    let paint = paint_of(page, tag, "circle")?;
    page.circle(cx, flip(cy), r, paint);
    Ok(())
}

fn draw_path(page: &mut Page, tag: &str, flip: impl Fn(f64) -> f64) -> Result<(), Svg2PdfError> {
    let d = attr(tag, "d").ok_or(Svg2PdfError::MissingAttribute {
        element: "path",
        attr: "d",
    })?;
    let ops = parse_path_data(d, &flip)?;
    let paint = paint_of(page, tag, "path")?;
    page.path(&ops, paint);
    Ok(())
}

fn draw_polyline(
    page: &mut Page,
    tag: &str,
    flip: impl Fn(f64) -> f64,
) -> Result<(), Svg2PdfError> {
    let points = attr(tag, "points").ok_or(Svg2PdfError::MissingAttribute {
        element: "polyline",
        attr: "points",
    })?;
    let mut ops = Vec::new();
    for (idx, pair) in points.split_whitespace().enumerate() {
        let (xs, ys) = pair
            .split_once(',')
            .ok_or_else(|| Svg2PdfError::MalformedAttribute {
                element: "polyline",
                attr: "points",
                value: points.to_string(),
            })?;
        let (x, y) = (
            parse_f64("polyline", "points", xs)?,
            parse_f64("polyline", "points", ys)?,
        );
        ops.push(if idx == 0 {
            PathOp::MoveTo(x, flip(y))
        } else {
            PathOp::LineTo(x, flip(y))
        });
    }
    // SVG fills a polyline as though closed even though its stroke is not —
    // real KiCad symbol polygons are always meant to read as closed shapes,
    // so this codebase's one caller wants exactly that.
    ops.push(PathOp::ClosePath);
    let paint = paint_of(page, tag, "polyline")?;
    page.path(&ops, paint);
    Ok(())
}

fn draw_text(
    page: &mut Page,
    tag: &str,
    content: &str,
    flip: impl Fn(f64) -> f64,
) -> Result<(), Svg2PdfError> {
    let x = req_f64(tag, "text", "x")?;
    let y = req_f64(tag, "text", "y")?;
    let size = attr(tag, "font-size")
        .and_then(|s| parse_f64("text", "font-size", s).ok())
        .unwrap_or(10.0);
    let font = match attr(tag, "font-weight") {
        Some(w) if w.parse::<u32>().is_ok_and(|w| w >= 600) => Font::Bold,
        _ => Font::Regular,
    };
    let (r, g, b) = match attr(tag, "fill") {
        Some(c) => parse_color(c)?.ok_or(Svg2PdfError::BadColor("none".into()))?,
        None => (0.0, 0.0, 0.0),
    };
    let width = content.chars().count() as f64 * size * AVG_CHAR_WIDTH;
    let dx = match attr(tag, "text-anchor") {
        Some("middle") => -width / 2.0,
        Some("end") => -width,
        _ => 0.0,
    };
    // Approximate cap-height vertical centering — see module docs.
    let dy = match attr(tag, "dominant-baseline") {
        Some("middle") => size * 0.32,
        _ => 0.0,
    };
    page.set_fill(r, g, b);
    page.text(x + dx, flip(y) - dy, size, font, content);
    Ok(())
}

/// The paint this codebase's SVG always carries: a `fill` (a color, or
/// `"none"`/absent) plus, on every shape but `<circle>`, a `stroke`. Circles
/// here are always solid dots with no separate stroke, so a missing
/// `stroke` attribute means fill-only rather than an error.
fn paint_of(page: &mut Page, tag: &str, element: &'static str) -> Result<Paint, Svg2PdfError> {
    let fill = match attr(tag, "fill") {
        Some(c) => parse_color(c)?,
        None => None,
    };
    let stroke = match attr(tag, "stroke") {
        Some(c) => parse_color(c)?,
        None => None,
    };
    match (fill, stroke) {
        (Some((fr, fg, fb)), Some((sr, sg, sb))) => {
            // PDF paints fill and stroke from separate current colors; set
            // both, then request the combined "B" op.
            page.set_fill(fr, fg, fb);
            page.set_stroke(sr, sg, sb);
            if let Some(w) = attr(tag, "stroke-width").and_then(|s| s.parse().ok()) {
                page.set_line_width(w);
            }
            Ok(Paint::FillStroke)
        }
        (Some((r, g, b)), None) => {
            page.set_fill(r, g, b);
            Ok(Paint::Fill)
        }
        (None, Some((r, g, b))) => {
            page.set_stroke(r, g, b);
            if let Some(w) = attr(tag, "stroke-width").and_then(|s| s.parse().ok()) {
                page.set_line_width(w);
            }
            Ok(Paint::Stroke)
        }
        (None, None) => Err(Svg2PdfError::MissingAttribute {
            element,
            attr: "fill/stroke",
        }),
    }
}

fn parse_color(s: &str) -> Result<Option<(f64, f64, f64)>, Svg2PdfError> {
    if s == "none" {
        return Ok(None);
    }
    let hex = s
        .strip_prefix('#')
        .ok_or_else(|| Svg2PdfError::BadColor(s.to_string()))?;
    if hex.len() != 6 {
        return Err(Svg2PdfError::BadColor(s.to_string()));
    }
    let byte = |i: usize| {
        u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| Svg2PdfError::BadColor(s.to_string()))
    };
    let (r, g, b) = (byte(0)?, byte(2)?, byte(4)?);
    Ok(Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)))
}

/// Parses an SVG path `d` string built only from `M`/`L`/`A` commands (see
/// module docs) into [`PathOp`]s, flipping every y-coordinate from SVG's
/// y-down space into PDF's y-up space.
fn parse_path_data(d: &str, flip: &impl Fn(f64) -> f64) -> Result<Vec<PathOp>, Svg2PdfError> {
    let mut ops = Vec::new();
    let mut chars = d.trim().chars().peekable();
    let mut cur = (0.0, 0.0);
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        chars.next();
        let rest: String = std::iter::from_fn(|| {
            chars
                .peek()
                .filter(|c| !c.is_alphabetic() || **c == 'e' || **c == 'E')
                .copied()
                .inspect(|_| {
                    chars.next();
                })
        })
        .collect();
        let nums: Vec<f64> = rest
            .split_whitespace()
            .flat_map(|tok| tok.split(','))
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<f64>()
                    .map_err(|_| Svg2PdfError::BadPathData(d.to_string()))
            })
            .collect::<Result<_, _>>()?;
        match c {
            'M' => {
                let [x, y] = nums[..] else {
                    return Err(Svg2PdfError::BadPathData(d.to_string()));
                };
                cur = (x, y);
                ops.push(PathOp::MoveTo(x, flip(y)));
            }
            'L' => {
                for pair in nums.chunks(2) {
                    let [x, y] = pair else {
                        return Err(Svg2PdfError::BadPathData(d.to_string()));
                    };
                    cur = (*x, *y);
                    ops.push(PathOp::LineTo(*x, flip(*y)));
                }
            }
            'A' => {
                let [rx, ry, rot, large, sweep, x, y] = nums[..] else {
                    return Err(Svg2PdfError::BadPathData(d.to_string()));
                };
                ops.push(PathOp::ArcTo {
                    rx,
                    ry,
                    x_rotation_deg: rot,
                    large_arc: large != 0.0,
                    sweep: sweep != 0.0,
                    x,
                    y: flip(y),
                });
                cur = (x, y);
            }
            other => {
                return Err(Svg2PdfError::BadPathData(format!(
                    "unsupported command '{other}' in {d:?}"
                )))
            }
        }
    }
    let _ = cur;
    Ok(ops)
}

fn req_f64(tag: &str, element: &'static str, attr_name: &'static str) -> Result<f64, Svg2PdfError> {
    let raw = attr(tag, attr_name).ok_or(Svg2PdfError::MissingAttribute {
        element,
        attr: attr_name,
    })?;
    parse_f64(element, attr_name, raw)
}

fn opt_f64(
    tag: &str,
    element: &'static str,
    attr_name: &'static str,
) -> Result<Option<f64>, Svg2PdfError> {
    attr(tag, attr_name)
        .map(|raw| parse_f64(element, attr_name, raw))
        .transpose()
}

fn parse_f64(
    element: &'static str,
    attr_name: &'static str,
    raw: &str,
) -> Result<f64, Svg2PdfError> {
    raw.parse().map_err(|_| Svg2PdfError::MalformedAttribute {
        element,
        attr: attr_name,
        value: raw.to_string(),
    })
}

/// Extracts `name="value"` from a tag's raw text. This codebase's own SVG
/// generator always double-quotes attribute values and never nests a
/// literal `"` inside one (text content is XML-escaped via `&quot;`), so a
/// straightforward `name="..."` scan is sufficient — this is not a general
/// SVG/XML attribute parser.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let mut search_from = 0;
    while let Some(rel) = tag[search_from..].find(&needle) {
        let idx = search_from + rel;
        // Guard against matching a longer attribute's suffix (e.g. "x" inside "stroke-width").
        let boundary_ok = idx == 0
            || tag[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || c == '<');
        let value_start = idx + needle.len();
        if boundary_ok {
            let value_end = tag[value_start..].find('"')? + value_start;
            return Some(&tag[value_start..value_end]);
        }
        search_from = value_start;
    }
    None
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_finds_a_simple_value() {
        assert_eq!(attr(r#"<rect x="1.0" y="2.0"/>"#, "x"), Some("1.0"));
        assert_eq!(attr(r#"<rect x="1.0" y="2.0"/>"#, "y"), Some("2.0"));
    }

    #[test]
    fn attr_does_not_match_a_suffix_of_a_longer_name() {
        let tag = r##"<path stroke-width="1.4" stroke="#2a2a28"/>"##;
        assert_eq!(attr(tag, "stroke"), Some("#2a2a28"));
        assert_eq!(attr(tag, "width"), None);
    }

    #[test]
    fn attr_missing_is_none() {
        assert_eq!(attr(r#"<rect x="1.0"/>"#, "fill"), None);
    }

    #[test]
    fn parses_a_hex_color() {
        assert_eq!(
            parse_color("#2a2a28").unwrap(),
            Some((
                0x2a as f64 / 255.0,
                0x2a as f64 / 255.0,
                0x28 as f64 / 255.0
            ))
        );
    }

    #[test]
    fn none_color_is_no_paint() {
        assert_eq!(parse_color("none").unwrap(), None);
    }

    #[test]
    fn bad_color_fails_loud() {
        assert!(parse_color("red").is_err());
    }

    #[test]
    fn parses_move_and_line_path_data() {
        let flip = |y: f64| 100.0 - y;
        let ops = parse_path_data("M10.0 20.0 L30.0 40.0", &flip).unwrap();
        assert!(matches!(ops[0], PathOp::MoveTo(x, y) if x == 10.0 && y == 80.0));
        assert!(matches!(ops[1], PathOp::LineTo(x, y) if x == 30.0 && y == 60.0));
    }

    #[test]
    fn parses_an_arc_command() {
        let flip = |y: f64| 100.0 - y;
        let ops = parse_path_data("M0.0 0.0 A5.0 5.0 0 0 1 10.0 0.0", &flip).unwrap();
        assert!(matches!(
            ops[1],
            PathOp::ArcTo {
                rx: 5.0,
                ry: 5.0,
                large_arc: false,
                sweep: true,
                x: 10.0,
                ..
            }
        ));
    }

    #[test]
    fn unsupported_command_fails_loud_not_silently() {
        let flip = |y: f64| y;
        assert!(parse_path_data("M0 0 C1 1 2 2 3 3", &flip).is_err());
    }

    #[test]
    fn xml_unescape_reverses_the_schematic_module_escaper() {
        assert_eq!(
            xml_unescape("R1 &amp; C1 &lt;fuzz&gt; &quot;v1&quot;"),
            "R1 & C1 <fuzz> \"v1\""
        );
    }

    #[test]
    fn translates_a_minimal_real_shaped_svg_without_error() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="100" height="100" role="img" aria-label="demo schematic"><rect width="100" height="100" fill="#fbfbf7"/><path d="M10.0 10.0 L90.0 10.0" stroke="#2a2a28" stroke-width="1.3" fill="none"/><circle cx="50.0" cy="50.0" r="2.6" fill="#2a2a28"/><text x="10.0" y="20.0" font-family="ui-monospace,monospace" font-size="12" font-weight="600" fill="#2a2a28" text-anchor="middle">R1</text></svg>"##;
        let (_, w, h) = svg_to_page(svg).unwrap();
        assert_eq!((w, h), (100.0, 100.0));
    }

    #[test]
    fn produces_a_real_pdf_byte_stream() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 50 50" width="50" height="50"><rect x="0" y="0" width="50" height="50" fill="#ffffff" stroke="#000000" stroke-width="1"/></svg>"##;
        let bytes = svg_to_pdf_bytes(svg).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
    }

    /// The real round trip: `schematic_to_svg`'s own output (fallback-box
    /// rendering, since this circuit has no resolved KiCad symbols) through
    /// this module, unmodified — not a hand-written fixture standing in for
    /// it.
    #[test]
    fn translates_real_schematic_to_svg_output() {
        use crate::model::{Circuit, Net, Part, PinRef};
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
        let svg = crate::schematic::schematic_to_svg(&c);
        let bytes = svg_to_pdf_bytes(&svg).expect("real schematic SVG should translate cleanly");
        assert!(bytes.starts_with(b"%PDF"));
    }
}
