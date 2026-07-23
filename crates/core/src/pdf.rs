//! A tiny, dependency-free PDF writer — A4 pages with filled/stroked rectangles,
//! circles, and Helvetica text. Just enough for the build-guide export
//! ([`guide`](crate::guide)): deterministic and self-contained, so PDF output
//! never depends on a headless browser or a heavy crate.

use std::fmt::Write;

/// PDF user-space units per millimetre (1 pt = 1/72 inch).
pub const MM: f64 = 72.0 / 25.4;
/// A4 page size in points.
pub const A4_W: f64 = 210.0 * MM;
pub const A4_H: f64 = 297.0 * MM;

/// Which built-in font a text run uses.
#[derive(Clone, Copy)]
pub enum Font {
    Regular,
    Bold,
}

/// How a shape is painted.
#[derive(Clone, Copy)]
pub enum Paint {
    Fill,
    Stroke,
    FillStroke,
}

impl Paint {
    fn op(self) -> &'static str {
        match self {
            Paint::Fill => "f",
            Paint::Stroke => "S",
            Paint::FillStroke => "B",
        }
    }
}

/// A JPEG image, embeddable directly via PDF's `DCTDecode` filter.
pub struct Image {
    jpeg: Vec<u8>,
    w: u32,
    h: u32,
}

impl Image {
    /// Read a JPEG's pixel dimensions from its `SOFn` frame header.
    pub fn from_jpeg(jpeg: Vec<u8>) -> Option<Image> {
        let b = &jpeg;
        if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
            return None;
        }
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            // Start-of-frame markers carry the dimensions (skip DHT 0xC4 etc.).
            if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                let h = ((b[i + 5] as u32) << 8) | b[i + 6] as u32;
                let w = ((b[i + 7] as u32) << 8) | b[i + 8] as u32;
                return Some(Image { jpeg, w, h });
            }
            if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = ((b[i + 2] as usize) << 8) | b[i + 3] as usize;
            i += 2 + len;
        }
        None
    }
}

/// One page's content stream, built with drawing calls (PDF coords: origin at the
/// bottom-left, Y up, units = points).
#[derive(Default)]
pub struct Page {
    ops: String,
}

impl Page {
    pub fn new() -> Self {
        Page::default()
    }

    pub fn set_fill(&mut self, r: f64, g: f64, b: f64) {
        let _ = writeln!(self.ops, "{r:.3} {g:.3} {b:.3} rg");
    }
    pub fn set_stroke(&mut self, r: f64, g: f64, b: f64) {
        let _ = writeln!(self.ops, "{r:.3} {g:.3} {b:.3} RG");
    }
    pub fn set_line_width(&mut self, w: f64) {
        let _ = writeln!(self.ops, "{w:.3} w");
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, paint: Paint) {
        let _ = writeln!(self.ops, "{x:.2} {y:.2} {w:.2} {h:.2} re {}", paint.op());
    }

    /// A circle centred at `(cx, cy)`, radius `r`, via four Bézier arcs.
    pub fn circle(&mut self, cx: f64, cy: f64, r: f64, paint: Paint) {
        let k = 0.552_284_75 * r;
        let _ = writeln!(
            self.ops,
            "{:.2} {:.2} m \
             {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c \
             {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c \
             {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c \
             {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} c {}",
            cx + r,
            cy,
            cx + r,
            cy + k,
            cx + k,
            cy + r,
            cx,
            cy + r,
            cx - k,
            cy + r,
            cx - r,
            cy + k,
            cx - r,
            cy,
            cx - r,
            cy - k,
            cx - k,
            cy - r,
            cx,
            cy - r,
            cx + k,
            cy - r,
            cx + r,
            cy - k,
            cx + r,
            cy,
            paint.op()
        );
    }

    /// Draw the document's shared image (`/Im0`) under a `cm` transform, clipped
    /// to the rectangle `clip` (so an oversized image shows only there).
    pub fn draw_image(&mut self, cm: [f64; 6], clip: (f64, f64, f64, f64)) {
        let [a, b, c, d, e, ff] = cm;
        let (cx, cy, cw, ch) = clip;
        let _ = writeln!(
            self.ops,
            "q {cx:.2} {cy:.2} {cw:.2} {ch:.2} re W n \
             {a:.5} {b:.5} {c:.5} {d:.5} {e:.3} {ff:.3} cm /Im0 Do Q"
        );
    }

    /// Left-anchored text at baseline `(x, y)`.
    pub fn text(&mut self, x: f64, y: f64, size: f64, font: Font, s: &str) {
        let f = match font {
            Font::Regular => "F1",
            Font::Bold => "F2",
        };
        let _ = writeln!(
            self.ops,
            "BT /{f} {size:.1} Tf {x:.2} {y:.2} Td ({}) Tj ET",
            escape(s)
        );
    }
}

/// Escape a string for a PDF literal `( … )`.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            // Non-ASCII would need proper encoding; approximate with '?'.
            c if (c as u32) < 128 => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

/// Assemble `pages` into a complete PDF document (A4). `image`, if present, is
/// embedded once as the shared `/Im0` XObject and available to every page's
/// [`draw_image`](Page::draw_image).
pub fn document(pages: &[Page], image: Option<&Image>) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &str| {
        offsets.push(out.len());
        // Object N is the Nth offset pushed; xref maps entry N → offsets[N-1].
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", offsets.len()).as_bytes());
    };
    // The shared image XObject (if any) is appended after the page/content objects.
    let img_obj = 5 + 2 * pages.len();
    let xobject = if image.is_some() {
        format!(" /XObject << /Im0 {img_obj} 0 R >>")
    } else {
        String::new()
    };

    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");

    // 1: catalog, 2: pages, 3/4: fonts. Page objects then content objects follow.
    obj(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    // The pages object references each page object (obj 5, 7, 9, …).
    let kids: String = (0..pages.len())
        .map(|i| format!("{} 0 R", 5 + 2 * i))
        .collect::<Vec<_>>()
        .join(" ");
    obj(
        &mut out,
        &mut offsets,
        &format!("<< /Type /Pages /Kids [ {kids} ] /Count {} >>", pages.len()),
    );
    obj(
        &mut out,
        &mut offsets,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    );
    obj(
        &mut out,
        &mut offsets,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>",
    );

    for (i, page) in pages.iter().enumerate() {
        let content_obj = 6 + 2 * i;
        obj(
            &mut out,
            &mut offsets,
            &format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {A4_W:.2} {A4_H:.2}] \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R >>{xobject} >> \
                 /Contents {content_obj} 0 R >>"
            ),
        );
        // Content stream object.
        offsets.push(out.len());
        let stream = &page.ops;
        out.extend_from_slice(
            format!(
                "{} 0 obj\n<< /Length {} >>\nstream\n{stream}endstream\nendobj\n",
                offsets.len(),
                stream.len()
            )
            .as_bytes(),
        );
    }

    // Shared image XObject (raw JPEG via DCTDecode).
    if let Some(img) = image {
        offsets.push(out.len());
        out.extend_from_slice(
            format!(
                "{} 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} \
                 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\n\
                 stream\n",
                offsets.len(),
                img.w,
                img.h,
                img.jpeg.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&img.jpeg);
        out.extend_from_slice(b"\nendstream\nendobj\n");
    }

    // Cross-reference table.
    let xref_at = out.len();
    let count = offsets.len() + 1;
    out.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n")
            .as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_pdf_literals() {
        assert_eq!(escape("a(b)c\\d"), "a\\(b\\)c\\\\d");
    }

    #[test]
    fn document_is_a_valid_pdf_skeleton() {
        let mut p = Page::new();
        p.set_fill(1.0, 0.0, 0.0);
        p.rect(10.0, 10.0, 50.0, 20.0, Paint::Fill);
        p.text(10.0, 40.0, 12.0, Font::Bold, "Step 1");
        let bytes = document(&[p], None);
        assert!(bytes.starts_with(b"%PDF-1.7"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("/Type /Catalog"));
        assert!(s.contains("startxref"));
        assert!(s.contains("(Step 1) Tj"));
        // Objects must be numbered 1..N with no gap — the catalog (`/Root 1 0 R`)
        // must actually be object 1, and every xref entry must resolve.
        assert!(s.contains("\n1 0 obj\n"), "catalog must be object 1");
        let n_objs = s.matches(" 0 obj\n").count();
        for k in 1..=n_objs {
            assert!(
                s.contains(&format!("\n{k} 0 obj\n")),
                "object {k} missing (numbering gap)"
            );
        }
        assert!(s.contains(&format!("/Size {}", n_objs + 1)));
    }
}
