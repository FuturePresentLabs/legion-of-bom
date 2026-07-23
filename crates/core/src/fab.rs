//! Fab package — the manufacturing outputs you upload to order a board (nxo.4).
//!
//! Gerbers + drill + a JLCPCB-format CPL (pick-and-place) and BOM, produced from
//! a DRC-clean `.kicad_pcb`. The CLI (`lob fab`) generates the board, gates it on
//! DRC ([`run_drc`](crate::drc::run_drc)), then calls these to write the package.
//!
//! The reformatting to JLCPCB's column layouts is pure and unit-tested; the
//! Gerber/drill/position exports shell out to `kicad-cli`.

use std::path::Path;
use std::process::Command;

use crate::bom::Bom;
use crate::stage::StageError;

/// Run a `kicad-cli` subcommand, mapping failure to a [`StageError`].
fn run_kicad(kicad_cli: &Path, args: &[&str], board: &Path) -> Result<(), StageError> {
    let output = Command::new(kicad_cli)
        .args(args)
        .arg(board)
        .output()
        .map_err(|e| StageError::ToolNotFound(format!("kicad-cli: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(StageError::ToolFailed {
            tool: format!("kicad-cli {}", args.join(" ")),
            code: output.status.code().unwrap_or(-1),
            stderr: stderr.lines().rev().take(8).collect::<Vec<_>>().join("\n"),
        });
    }
    Ok(())
}

/// Export Gerbers (zones filled) + drill files into `out_dir`. JLCPCB accepts a
/// zip of this directory.
pub fn export_gerbers(board: &Path, out_dir: &Path, kicad_cli: &Path) -> Result<(), StageError> {
    std::fs::create_dir_all(out_dir)?;
    // A trailing separator tells kicad-cli the target is a directory.
    let dir = format!("{}{}", out_dir.display(), std::path::MAIN_SEPARATOR);
    run_kicad(
        kicad_cli,
        &[
            "pcb",
            "export",
            "gerbers",
            "--check-zones",
            "--use-drill-file-origin",
            "-o",
            &dir,
        ],
        board,
    )?;
    run_kicad(
        kicad_cli,
        &[
            "pcb",
            "export",
            "drill",
            "--drill-origin",
            "plot",
            "-o",
            &dir,
        ],
        board,
    )?;
    Ok(())
}

/// Export a top-view SVG of the real board (copper, silk, fab outlines, edge) for
/// the build-guide underlay ([`guide`](crate::guide)). Real board coordinates
/// (mm) are preserved, so the guide's highlight boxes overlay accurately.
pub fn export_board_svg(board: &Path, kicad_cli: &Path) -> Result<String, StageError> {
    let stem = board
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board");
    let out = std::env::temp_dir().join(format!("lob-{stem}-guide.svg"));
    run_kicad(
        kicad_cli,
        &[
            "pcb",
            "export",
            "svg",
            "--layers",
            "F.Cu,B.Cu,F.Silkscreen,F.Fab,Edge.Cuts",
            "--exclude-drawing-sheet",
            "--mode-single",
            "-o",
            out.to_str().unwrap_or_default(),
        ],
        board,
    )?;
    Ok(std::fs::read_to_string(&out)?)
}

/// Render the real board to a JPEG (A4-landscape page, true mm coords) for the
/// PDF build guide's diagram underlay ([`guide::guide_to_pdf`](crate::guide)).
///
/// KiCad plots the board to a PDF (copper/silk/fab/edge, board at its real page
/// position); QuickLook rasterizes that at high resolution and `sips` converts to
/// JPEG (the only raster our [`pdf`](crate::pdf) writer embeds). Falls back to a
/// direct `sips` PDF→JPEG (lower resolution) if QuickLook is unavailable, and
/// surfaces a [`StageError`] if neither tool works — so the caller drops to the
/// schematic diagram rather than failing the guide.
pub fn render_board_jpeg(board: &Path, kicad_cli: &Path) -> Result<Vec<u8>, StageError> {
    let stem = board
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board");
    let tmp = std::env::temp_dir();
    let plot = tmp.join(format!("lob-{stem}-plot.pdf"));
    run_kicad(
        kicad_cli,
        &[
            "pcb",
            "export",
            "pdf",
            // Copper + silkscreen (outlines/refdes) + edge; F.Fab is dropped —
            // its footprint-name text clutters the raster at guide scale.
            "--layers",
            "F.Cu,B.Cu,F.Silkscreen,Edge.Cuts",
            "--mode-single",
            "-o",
            plot.to_str().unwrap_or_default(),
        ],
        board,
    )?;
    let jpg = tmp.join(format!("lob-{stem}-plot.jpg"));
    if let Some(bytes) = rasterize_via_qlmanage(&plot, &tmp, &jpg) {
        return Ok(bytes);
    }
    rasterize_via_sips(&plot, &jpg)
}

/// Rasterize `pdf` at high resolution via macOS QuickLook (`qlmanage`) then
/// convert the PNG to JPEG with `sips`. Returns `None` if either step is missing
/// or fails, so the caller can fall back.
fn rasterize_via_qlmanage(pdf: &Path, outdir: &Path, jpg: &Path) -> Option<Vec<u8>> {
    let name = pdf.file_name()?.to_str()?;
    let png = outdir.join(format!("{name}.png"));
    let _ = std::fs::remove_file(&png);
    let ok = Command::new("qlmanage")
        .args(["-t", "-s", "3000", "-o"])
        .arg(outdir)
        .arg(pdf)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok || !png.is_file() {
        return None;
    }
    let ok = Command::new("sips")
        .args(["-s", "format", "jpeg"])
        .arg(&png)
        .arg("--out")
        .arg(jpg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    std::fs::read(jpg).ok()
}

/// Rasterize `pdf` directly to JPEG with `sips` (72 DPI — lower resolution than
/// the QuickLook path, but a dependency-light fallback).
fn rasterize_via_sips(pdf: &Path, jpg: &Path) -> Result<Vec<u8>, StageError> {
    let out = Command::new("sips")
        .args(["-s", "format", "jpeg"])
        .arg(pdf)
        .arg("--out")
        .arg(jpg)
        .output()
        .map_err(|e| StageError::ToolNotFound(format!("sips: {e}")))?;
    if !out.status.success() {
        return Err(StageError::ToolFailed {
            tool: "sips".into(),
            code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }
    Ok(std::fs::read(jpg)?)
}

/// Zip `dir`'s contents (flat, files at the archive root) into `zip_path` — the
/// form JLCPCB accepts for PCB upload. Uses the system `zip`; returns `false`
/// (rather than erroring) if it isn't available, so the caller can fall back to
/// pointing at the directory.
pub fn zip_dir(dir: &Path, zip_path: &Path) -> Result<bool, StageError> {
    let _ = std::fs::remove_file(zip_path); // start a fresh archive
    let abs = std::path::absolute(zip_path)?;
    // Run from inside `dir` and archive ".", so the gerbers sit at the zip root.
    let status = Command::new("zip")
        .args(["-r", "-q"])
        .arg(&abs)
        .arg(".")
        .current_dir(dir)
        .status();
    Ok(matches!(status, Ok(s) if s.success()) && zip_path.is_file())
}

/// Write a JLCPCB CPL (pick-and-place) by reformatting `kicad-cli`'s position
/// export. Returns the number of placed components.
pub fn export_cpl(board: &Path, out: &Path, kicad_cli: &Path) -> Result<usize, StageError> {
    let stem = board
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board");
    let tmp = std::env::temp_dir().join(format!("lob-{stem}-pos.csv"));
    run_kicad(
        kicad_cli,
        &[
            "pcb",
            "export",
            "pos",
            "--format",
            "csv",
            "--units",
            "mm",
            "--use-drill-file-origin",
            "-o",
            tmp.to_str().unwrap_or_default(),
        ],
        board,
    )?;
    let pos = std::fs::read_to_string(&tmp)?;
    let (csv, n) = jlc_cpl_from_kicad_pos(&pos);
    std::fs::write(out, csv)?;
    Ok(n)
}

/// Reformat `kicad-cli pcb export pos` CSV (`Ref,Val,Package,PosX,PosY,Rot,Side`)
/// into JLCPCB's CPL (`Designator,Mid X,Mid Y,Layer,Rotation`). Returns the CSV
/// and the row count.
pub fn jlc_cpl_from_kicad_pos(pos_csv: &str) -> (String, usize) {
    let mut out = String::from("Designator,Mid X,Mid Y,Layer,Rotation\n");
    let mut n = 0;
    for line in pos_csv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f = parse_csv_row(line);
        if f.len() < 7 {
            continue;
        }
        let layer = if f[6].eq_ignore_ascii_case("bottom") {
            "Bottom"
        } else {
            "Top"
        };
        out.push_str(&format!(
            "{},{},{},{},{}\n",
            csv_field(&f[0]),
            f[3],
            f[4],
            layer,
            f[5]
        ));
        n += 1;
    }
    (out, n)
}

/// Format a [`Bom`] as a JLCPCB assembly BOM
/// (`Comment,Designator,Footprint,LCSC Part #`). Parts are already grouped by
/// [`BomLine`](crate::bom::BomLine); the footprint short name (after `lib:`) is
/// used, and the MPN goes in the LCSC column when present.
pub fn jlc_bom_csv(bom: &Bom) -> String {
    let mut out = String::from("Comment,Designator,Footprint,LCSC Part #\n");
    for line in &bom.lines {
        let designators = line.refdes.join(", ");
        let footprint = line
            .footprint
            .as_deref()
            .map(|f| f.rsplit(':').next().unwrap_or(f))
            .unwrap_or("");
        out.push_str(&format!(
            "{},{},{},{}\n",
            csv_field(&line.value),
            csv_field(&designators),
            csv_field(footprint),
            csv_field(line.mpn.as_deref().unwrap_or("")),
        ));
    }
    out
}

/// Quote a CSV field if it contains a comma, quote, or newline (RFC 4180).
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Parse one CSV row, honouring `"..."` quoting (with `""` escapes).
fn parse_csv_row(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bom::{Bom, BomLine};

    #[test]
    fn cpl_reformats_to_jlc_columns() {
        let pos = "Ref,Val,Package,PosX,PosY,Rot,Side\n\
                   \"C1\",\"159n\",\"C_0805\",100.0,-50.0,90.0,top\n\
                   \"J1\",\"Conn\",\"Hdr\",103.0,-50.0,0.0,bottom\n";
        let (csv, n) = jlc_cpl_from_kicad_pos(pos);
        assert_eq!(n, 2);
        assert!(csv.starts_with("Designator,Mid X,Mid Y,Layer,Rotation\n"));
        assert!(csv.contains("C1,100.0,-50.0,Top,90.0"));
        assert!(csv.contains("J1,103.0,-50.0,Bottom,0.0"));
    }

    #[test]
    fn bom_formats_for_jlc_with_grouped_designators() {
        let bom = Bom {
            lines: vec![
                BomLine {
                    mpn: None,
                    value: "159n".into(),
                    footprint: Some("Capacitor_SMD:C_0805_2012Metric".into()),
                    refdes: vec!["C1".into(), "C5".into()],
                    unit_price: None,
                    ext_price: None,
                },
                BomLine {
                    mpn: Some("TL072CDR".into()),
                    value: "TL072".into(),
                    footprint: Some("Package_SO:SOIC-8".into()),
                    refdes: vec!["U1".into()],
                    unit_price: None,
                    ext_price: None,
                },
            ],
        };
        let csv = jlc_bom_csv(&bom);
        assert!(csv.starts_with("Comment,Designator,Footprint,LCSC Part #\n"));
        // Multi-designator field is quoted (contains a comma); footprint short name.
        assert!(csv.contains("159n,\"C1, C5\",C_0805_2012Metric,\n"));
        assert!(csv.contains("TL072,U1,SOIC-8,TL072CDR\n"));
    }

    #[test]
    fn csv_quoting_roundtrips_commas() {
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(parse_csv_row("\"a,b\",c"), vec!["a,b", "c"]);
    }
}
