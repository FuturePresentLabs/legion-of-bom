//! Reading somebody else's finished board — a **fab package** import (u7p).
//!
//! The exchange format is deliberately not a CAD file. DipTrace's `.dch`/`.dip`
//! are proprietary binary, and Eagle/Altium each want their own reader; but every
//! one of them can already emit the package a board house is sent, and that
//! package is plain CSV plus gerbers — the same shape [`crate::fab`] writes. So an
//! import reads the *output* of someone's CAD tool rather than its project file,
//! which is stable, vendor-neutral, and needs no licence.
//!
//! **What this recovers:** the components (designator, value, footprint, and any
//! distributor part), and where each one sits (position, rotation, side).
//!
//! **What it cannot:** connectivity. A fab package has no netlist, so an imported
//! circuit can be seen, costed and re-ordered, but not simulated, ERC'd, or
//! re-laid-out. Callers should say so rather than present an empty schematic.

use std::path::{Path, PathBuf};

use crate::bom::{Bom, BomLine, LineKind};
use crate::guide::{detect_polarity, BuildGuide, PlacedPart};

/// One line of an imported BOM.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedPart {
    /// Reference designators this line covers (`C1`, `C5`, …).
    pub refdes: Vec<String>,
    /// The value/comment column, as the original author wrote it.
    pub value: String,
    /// Footprint name in the source tool's vocabulary — not necessarily one of
    /// ours, which is why an imported board is not re-laid-out.
    pub footprint: String,
    /// Distributor part number, when the package carried one (JLC's LCSC column).
    pub part_number: Option<String>,
    /// Marked "do not populate" by the author.
    pub dnp: bool,
}

/// Where one component sits, from the pick-and-place file.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedPlacement {
    pub refdes: String,
    pub x_mm: f64,
    pub y_mm: f64,
    pub rotation_deg: f64,
    /// `true` when the part is on the bottom of the board.
    pub back: bool,
}

/// A whole imported board.
#[derive(Debug, Clone, Default)]
pub struct ImportedBoard {
    pub parts: Vec<ImportedPart>,
    pub placements: Vec<ImportedPlacement>,
    /// Directory of gerbers, if the package shipped them unpacked.
    pub gerber_dir: Option<PathBuf>,
    /// Files that looked like part of the package but could not be read — kept so
    /// the caller can report an incomplete import rather than a silent one.
    pub skipped: Vec<String>,
}

impl ImportedBoard {
    /// Total component count, expanding grouped BOM lines and skipping DNPs.
    pub fn part_count(&self) -> usize {
        self.parts
            .iter()
            .filter(|p| !p.dnp)
            .map(|p| p.refdes.len())
            .sum()
    }
}

/// Split a CSV line, honouring double quotes (a BOM groups designators as
/// `"C1, C5"`, so a naive split on commas tears them apart).
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|f| f.trim().to_string()).collect()
}

/// Find a column by any of several header spellings — tools disagree on wording
/// ("Comment" vs "Value", "Mid X" vs "PosX") but agree on meaning.
fn column(headers: &[String], names: &[&str]) -> Option<usize> {
    headers.iter().position(|h| {
        let h = h.trim().to_ascii_lowercase();
        names.iter().any(|n| h == *n || h.starts_with(n))
    })
}

/// Like [`column`], but matching anywhere in the header.
///
/// The distributor column is the one nobody spells the same way, and its name is
/// often *prefixed*: `LCSC Part #`, `JLCPCB Part #`, `Supplier Part`. Matching on
/// a prefix alone silently loses the part numbers — which is worse than loud
/// failure, because the BOM still looks complete.
fn column_containing(headers: &[String], names: &[&str]) -> Option<usize> {
    headers.iter().position(|h| {
        let h = h.trim().to_ascii_lowercase();
        names.iter().any(|n| h.contains(n))
    })
}

/// Parse a JLC-style BOM CSV.
pub fn parse_bom(text: &str) -> Vec<ImportedPart> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let h = csv_fields(header);
    let (ci, di, fi, pi) = (
        column(&h, &["comment", "value"]),
        column(&h, &["designator", "refdes", "reference"]),
        column(&h, &["footprint", "package"]),
        column_containing(&h, &["lcsc", "part #", "part number", "mpn", "supplier"]),
    );
    let Some(di) = di else {
        return Vec::new();
    };

    lines
        .filter_map(|line| {
            let f = csv_fields(line);
            let get = |i: Option<usize>| i.and_then(|i| f.get(i)).cloned().unwrap_or_default();
            let refs: Vec<String> = get(Some(di))
                .split([',', ' '])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if refs.is_empty() {
                return None;
            }
            let value = get(ci);
            // Authors flag unpopulated parts in the value column (`*RST* DNP`).
            let dnp = value.to_ascii_uppercase().contains("DNP");
            Some(ImportedPart {
                refdes: refs,
                value,
                footprint: get(fi),
                part_number: Some(get(pi)).filter(|s| !s.is_empty()),
                dnp,
            })
        })
        .collect()
}

/// Parse a JLC-style pick-and-place (CPL) CSV.
pub fn parse_cpl(text: &str) -> Vec<ImportedPlacement> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let h = csv_fields(header);
    let (di, xi, yi, li, ri) = (
        column(&h, &["designator", "refdes", "reference"]),
        column(&h, &["mid x", "midx", "posx", "x"]),
        column(&h, &["mid y", "midy", "posy", "y"]),
        column(&h, &["layer", "side"]),
        column(&h, &["rotation", "rot"]),
    );
    let (Some(di), Some(xi), Some(yi)) = (di, xi, yi) else {
        return Vec::new();
    };

    lines
        .filter_map(|line| {
            let f = csv_fields(line);
            let get = |i: usize| f.get(i).cloned().unwrap_or_default();
            // Coordinates may carry a unit suffix (`14.287mm`).
            let num = |s: String| -> Option<f64> {
                let t: String = s
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
                    .collect();
                t.parse().ok()
            };
            let refdes = get(di);
            if refdes.is_empty() {
                return None;
            }
            Some(ImportedPlacement {
                refdes,
                x_mm: num(get(xi))?,
                y_mm: num(get(yi))?,
                rotation_deg: ri.and_then(|i| num(get(i))).unwrap_or(0.0),
                back: li
                    .map(|i| get(i).to_ascii_lowercase().starts_with('b'))
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Read a fab package from a directory: a BOM, a pick-and-place, and gerbers.
///
/// Files are matched by content-bearing name fragments rather than exact
/// filenames, because every project names them differently
/// (`PHRSR_2021_V7_JLCBOM.csv`, `scanner_REV2_JLCXY.csv`).
pub fn read_package(dir: &Path) -> std::io::Result<ImportedBoard> {
    let mut board = ImportedBoard::default();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();

    for path in &entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if path.is_dir() {
            // A directory of gerbers, however it's named.
            if lower.contains("gerber") || lower == "fab" {
                board.gerber_dir = Some(path.clone());
            }
            continue;
        }
        if !lower.ends_with(".csv") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            board.skipped.push(name.to_string());
            continue;
        };
        // Decide by header, not filename, so an oddly-named export still lands.
        let header = text.lines().next().unwrap_or("").to_ascii_lowercase();
        if header.contains("designator") && (header.contains("mid x") || header.contains("posx")) {
            board.placements = parse_cpl(&text);
        } else if header.contains("designator") || header.contains("comment") {
            board.parts = parse_bom(&text);
        }
    }

    // A package built from CAD sources rather than a fab house ships no
    // pick-and-place, but an Eagle board file states every part's position —
    // which is all the build guide needs. Only consulted as a fallback, since a
    // real CPL describes the board as actually manufactured.
    if board.placements.is_empty() {
        for path in &entries {
            let is_brd = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("brd"));
            if !is_brd {
                continue;
            }
            if let Ok(xml) = std::fs::read_to_string(path) {
                board.placements = crate::eagle::parse_board(&xml)
                    .into_iter()
                    .map(|p| ImportedPlacement {
                        refdes: p.refdes,
                        x_mm: p.x_mm,
                        y_mm: p.y_mm,
                        rotation_deg: p.rotation_deg,
                        back: p.back,
                    })
                    .collect();
                if !board.placements.is_empty() {
                    break;
                }
            }
        }
    }

    if board.parts.is_empty() && board.placements.is_empty() {
        board
            .skipped
            .push(format!("no BOM or pick-and-place CSV in {}", dir.display()));
    }
    Ok(board)
}

// ---------------------------------------------------------------------------
//  Inference from the package name — what a fab package doesn't say outright
// ---------------------------------------------------------------------------

/// Whether a package is hand-soldered through-hole rather than surface-mount.
///
/// A fab package never states this, but the assembly instructions turn on it:
/// a through-hole part is one the builder fits and solders, an SMD part usually
/// arrives already placed. The package name is the only evidence there is, so
/// this reads it — leaning to SMD, since that is the default for a JLC-assembled
/// board and a wrongly-THT part would put a step in the guide that does not
/// exist.
pub fn package_is_through_hole(package: &str) -> bool {
    let p = package.trim().to_ascii_uppercase().replace([' ', '_'], "-");
    const THT: &[&str] = &[
        "DIP",
        "TO-92",
        "TO92",
        "TO-220",
        "TO220",
        "RADIAL",
        "AXIAL",
        "HC49",
        "PTH",
        "THT",
        "HEADER",
        "PINHEADER",
        "SIP",
        "TRIMPOT",
        "POT",
        "JACK",
        "SWITCH",
        "TERMINAL",
        "SOCKET",
        "LED3MM",
        "LED5MM",
        "-3MM",
        "-5MM",
        "1X0",
        "2X0",
        "2X1",
        "2X3",
        "2X5",
        "1X2",
        "1X3",
        // Eurorack power. A shrouded 2x5 IDC header is always through-hole, and
        // calling it surface-mount drops the one part the build guide must open
        // with — it is soldered while the board still lies flat.
        "EURO",
        "IDC",
        "SHROUD",
        "POWER",
        "10P",
        "16P",
    ];
    THT.iter().any(|k| p.contains(k))
}

/// Nominal body size in mm for a package, when the name implies one.
///
/// Used to give an imported part a plausible extent so a build-guide highlight
/// has something to draw. It is a nominal size, not measured geometry — an
/// imported board has no footprint library behind it.
pub fn package_size_mm(package: &str) -> Option<(f64, f64)> {
    let p = package.trim().to_ascii_uppercase();
    // Imperial chip sizes, the overwhelming majority of a module's parts.
    for (code, w, h) in [
        ("0201", 0.6, 0.3),
        ("0402", 1.0, 0.5),
        ("0603", 1.6, 0.8),
        ("0805", 2.0, 1.25),
        ("1206", 3.2, 1.6),
        ("1210", 3.2, 2.5),
        ("2010", 5.0, 2.5),
        ("2512", 6.3, 3.2),
    ] {
        if p.contains(code) {
            return Some((w, h));
        }
    }
    for (name, w, h) in [
        ("SOT-23", 2.9, 1.3),
        ("SOT23", 2.9, 1.3),
        ("SOD-123", 2.7, 1.6),
        ("SOIC-8", 4.9, 3.9),
        ("SOIC8", 4.9, 3.9),
        ("SO8", 4.9, 3.9),
        ("SOIC-14", 8.7, 3.9),
        ("SOIC-16", 9.9, 3.9),
        ("TSSOP", 5.0, 4.4),
        ("LQFP48", 7.0, 7.0),
        ("TQFP32", 7.0, 7.0),
        ("TO-92", 4.8, 3.8),
        ("HC49", 11.5, 4.5),
        ("LED3MM", 3.0, 3.0),
        ("LED5MM", 5.0, 5.0),
    ] {
        if p.contains(name) {
            return Some((w, h));
        }
    }
    None
}

impl ImportedBoard {
    /// The BOM, with each line's part number carried through as its MPN so it can
    /// be priced, photographed for a Visual BOM, and ordered.
    pub fn to_bom(&self) -> Bom {
        Bom {
            lines: self
                .parts
                .iter()
                .filter(|p| !p.dnp)
                .map(|p| {
                    let mut refdes = p.refdes.clone();
                    refdes.sort();
                    BomLine {
                        kind: LineKind::Component,
                        mpn: p.part_number.clone(),
                        value: p.value.clone(),
                        footprint: Some(p.footprint.clone()).filter(|f| !f.is_empty()),
                        refdes,
                        unit_price: None,
                        ext_price: None,
                        image_url: None,
                    }
                })
                .collect(),
        }
    }

    /// The board's parts as the build guide sees them: position and side from the
    /// pick-and-place, value and package from the BOM, and everything else
    /// inferred from the package, since an imported board brings no footprint
    /// library with it.
    pub fn to_placed_parts(&self) -> Vec<PlacedPart> {
        // refdes -> (value, package), expanding the BOM's grouped lines.
        let mut meta: std::collections::HashMap<&str, (&str, &str)> =
            std::collections::HashMap::new();
        for p in &self.parts {
            for r in &p.refdes {
                meta.insert(r.as_str(), (p.value.as_str(), p.footprint.as_str()));
            }
        }

        self.placements
            .iter()
            .map(|pl| {
                let (value, package) = meta.get(pl.refdes.as_str()).copied().unwrap_or(("", ""));
                let (w, h) = package_size_mm(package).unwrap_or((2.0, 1.5));
                // A quarter turn swaps the body's footprint on the board.
                let quarter = ((pl.rotation_deg / 90.0).round() as i64).rem_euclid(2) != 0;
                let (w, h) = if quarter { (h, w) } else { (w, h) };
                PlacedPart {
                    refdes: pl.refdes.clone(),
                    value: value.to_string(),
                    footprint: package.to_string(),
                    cx: pl.x_mm,
                    cy: pl.y_mm,
                    bbox: (
                        pl.x_mm - w / 2.0,
                        pl.y_mm - h / 2.0,
                        pl.x_mm + w / 2.0,
                        pl.y_mm + h / 2.0,
                    ),
                    back: pl.back,
                    through_hole: package_is_through_hole(package),
                    // A fab package records no pad geometry, so there is no pin-1
                    // position to point at; the polarity note still applies.
                    pin1: None,
                    polarity: detect_polarity(&pl.refdes, package),
                }
            })
            .collect()
    }

    /// A DIY build guide for an imported board.
    ///
    /// The sequencing is the same one a board we laid out gets — back side first,
    /// low-profile to tall — because that ordering is a property of the parts, not
    /// of where they were read from. What differs is the evidence: position and
    /// side come from the pick-and-place, and everything the guide would normally
    /// take from a footprint library is inferred from the package name.
    pub fn to_guide(&self, name: &str) -> BuildGuide {
        self.to_guide_with(name, crate::guide::GuideOptions::default())
    }

    /// [`Self::to_guide`], with control over what the guide covers.
    pub fn to_guide_with(&self, name: &str, opts: crate::guide::GuideOptions) -> BuildGuide {
        crate::guide::guide_from_parts_with(name, self.to_placed_parts(), self.outline(), opts)
    }

    /// Board extent from the placements, as the guide's outline.
    pub fn outline(&self) -> (f64, f64, f64, f64) {
        let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in &self.placements {
            b = (
                b.0.min(p.x_mm),
                b.1.min(p.y_mm),
                b.2.max(p.x_mm),
                b.3.max(p.y_mm),
            );
        }
        if b.0 > b.2 {
            (0.0, 0.0, 10.0, 10.0)
        } else {
            // A little air, since parts sit inside the board rather than on its edge.
            (b.0 - 2.0, b.1 - 2.0, b.2 + 2.0, b.3 + 2.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BOM groups designators in one quoted field; splitting naively on commas
    /// would turn `"C1, C5"` into two broken columns.
    #[test]
    fn bom_groups_quoted_designators() {
        let csv = "Comment,Designator,Footprint,LCSC Part #（optional）\n\
                   10uF 50v,\"C1, C5\",CAP_1206,C12345\n\
                   100nF 50v,C2,0603 CAP,\n\
                   *RST* DNP,C8,0603 CAP,\n";
        let parts = parse_bom(csv);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].refdes, vec!["C1", "C5"]);
        assert_eq!(parts[0].value, "10uF 50v");
        assert_eq!(parts[0].part_number.as_deref(), Some("C12345"));
        assert!(parts[1].part_number.is_none(), "blank column is not a part");
        assert!(parts[2].dnp, "author marked it DNP");
    }

    /// Nobody spells the distributor column the same way, and it is usually
    /// prefixed — `LCSC Part #` here, `JLCPCB Part #` there. Matching on a prefix
    /// alone loses every part number while the BOM still looks complete.
    #[test]
    fn finds_the_part_number_column_however_it_is_spelled() {
        for header in [
            "Comment,Designator,Footprint,LCSC Part #（optional）",
            "Comment,Designator,Footprint,JLCPCB Part #",
            "Comment,Designator,Footprint,Supplier Part Number",
            "Value,Reference,Package,MPN",
        ] {
            let csv = format!("{header}\n1k,R1,0603,C11702\n");
            let parts = parse_bom(&csv);
            assert_eq!(
                parts[0].part_number.as_deref(),
                Some("C11702"),
                "header: {header}"
            );
        }
    }

    #[test]
    fn cpl_reads_position_rotation_and_side() {
        let csv = "Designator,Mid X,Mid Y,Layer,Rotation\n\
                   C1,14.287,23.656,Bottom,180.0\n\
                   R2,15.557mm,31.115mm,Top,0.0\n";
        let ps = parse_cpl(csv);
        assert_eq!(ps.len(), 2);
        assert!(ps[0].back, "Bottom layer");
        assert!((ps[0].rotation_deg - 180.0).abs() < 1e-9);
        assert!(!ps[1].back);
        // A unit suffix on the coordinate must not defeat the parse.
        assert!((ps[1].x_mm - 15.557).abs() < 1e-9, "{:?}", ps[1]);
    }

    /// Column names differ between tools; meaning does not.
    #[test]
    fn accepts_alternative_column_names() {
        let bom = "Value,Reference,Package\n10k,R1,0805\n";
        let parts = parse_bom(bom);
        assert_eq!(parts[0].value, "10k");
        assert_eq!(parts[0].footprint, "0805");

        let cpl = "Reference,PosX,PosY,Side,Rot\nR1,1.0,2.0,bottom,90\n";
        let ps = parse_cpl(cpl);
        assert_eq!(ps[0].refdes, "R1");
        assert!(ps[0].back && (ps[0].rotation_deg - 90.0).abs() < 1e-9);
    }

    #[test]
    fn part_count_expands_groups_and_skips_dnp() {
        let csv = "Comment,Designator,Footprint\n\
                   1u,\"C1, C2, C3\",0603\n\
                   DNP,C9,0603\n";
        let b = ImportedBoard {
            parts: parse_bom(csv),
            ..Default::default()
        };
        assert_eq!(b.part_count(), 3, "three populated, one DNP skipped");
    }

    /// A fab package never says whether a part is hand-soldered; the package
    /// name is the only evidence, and the guide's steps depend on it.
    #[test]
    fn infers_through_hole_from_the_package() {
        for p in [
            "DIP-8", "TO-92", "HC49UP", "1X03", "2X5-1.27", "Trimpot", "LED3MM",
        ] {
            assert!(
                package_is_through_hole(p),
                "{p} should read as through-hole"
            );
        }
        for p in ["0603", "CAP_1206", "SOT23-5", "SOIC-8", "LQFP48", "TSSOP14"] {
            assert!(!package_is_through_hole(p), "{p} should read as SMD");
        }
    }

    #[test]
    fn infers_a_nominal_body_size() {
        assert_eq!(package_size_mm("0603 CAP"), Some((1.6, 0.8)));
        assert_eq!(package_size_mm("CAP_1206"), Some((3.2, 1.6)));
        assert_eq!(package_size_mm("SOIC-8"), Some((4.9, 3.9)));
        assert_eq!(package_size_mm("MYSTERY"), None);
    }

    /// The part number is what makes an imported BOM orderable and gives the
    /// Visual BOM something to photograph, so it must survive as the MPN.
    #[test]
    fn bom_carries_part_numbers_and_drops_dnp() {
        let b = ImportedBoard {
            parts: parse_bom(
                "Comment,Designator,Footprint,LCSC Part #\n\
                 10uF,\"C1, C5\",CAP_1206,C12345\n\
                 DNP,C8,0603,\n",
            ),
            ..Default::default()
        };
        let bom = b.to_bom();
        assert_eq!(bom.lines.len(), 1, "the DNP line is not ordered");
        assert_eq!(bom.lines[0].mpn.as_deref(), Some("C12345"));
        assert_eq!(bom.lines[0].refdes, vec!["C1", "C5"]);
        assert_eq!(bom.lines[0].qty(), 2);
    }

    #[test]
    fn placed_parts_take_side_and_size_from_package_and_cpl() {
        let b = ImportedBoard {
            parts: parse_bom("Comment,Designator,Footprint\n1u,C1,0603\n8p,U1,DIP-8\n"),
            placements: parse_cpl(
                "Designator,Mid X,Mid Y,Layer,Rotation\n\
                 C1,10,20,Bottom,90\n\
                 U1,30,40,Top,0\n",
            ),
            ..Default::default()
        };
        let ps = b.to_placed_parts();
        let c1 = ps.iter().find(|p| p.refdes == "C1").unwrap();
        assert!(c1.back && !c1.through_hole);
        // 0603 is 1.6x0.8; turned a quarter it occupies 0.8x1.6.
        assert!((c1.bbox.2 - c1.bbox.0 - 0.8).abs() < 1e-9, "{:?}", c1.bbox);
        let u1 = ps.iter().find(|p| p.refdes == "U1").unwrap();
        assert!(u1.through_hole, "a DIP is hand-soldered");
    }

    #[test]
    fn empty_or_headerless_input_is_not_a_panic() {
        assert!(parse_bom("").is_empty());
        assert!(parse_cpl("").is_empty());
        assert!(parse_bom("nothing,useful\n1,2\n").is_empty());
    }
}
