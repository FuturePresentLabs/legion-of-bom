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
        column(&h, &["lcsc", "part", "mpn", "supplier"]),
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

    if board.parts.is_empty() && board.placements.is_empty() {
        board
            .skipped
            .push(format!("no BOM or pick-and-place CSV in {}", dir.display()));
    }
    Ok(board)
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

    #[test]
    fn empty_or_headerless_input_is_not_a_panic() {
        assert!(parse_bom("").is_empty());
        assert!(parse_cpl("").is_empty());
        assert!(parse_bom("nothing,useful\n1,2\n").is_empty());
    }
}
