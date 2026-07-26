//! Preview the printed build guide over a *real* board, offline-ish: parse an
//! existing `.net` + `.kicad_pcb` pair, render the bare board with `kicad-cli`
//! when it's installed, and write the HTML + PDF guide.
//!
//! This exists because the print layout can only be judged on paper — picture
//! size, sheet fill, whether a refdes is legible at 100% — and running the whole
//! SKiDL pipeline to see a CSS change is too slow a loop.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example guide_preview -- \
//!     out/slew_limiter/slew_limiter.net out/slew_limiter/slew_limiter.kicad_pcb /tmp/preview
//! ```

use std::path::PathBuf;

use legion_of_bom_core::{
    build_guide_with, guide_to_html, guide_to_pdf, kicad_cli_path, png_to_jpeg, render_board_png,
    BoardPng, GuideOptions, Quality,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [net, pcb, out] = <[String; 3]>::try_from(args)
        .map_err(|_| "usage: guide_preview <circuit.net> <board.kicad_pcb> <out-stem>")?;
    let (net, pcb, out) = (PathBuf::from(net), PathBuf::from(pcb), PathBuf::from(out));

    let circuit = legion_of_bom_core::parse_netlist_file(&net)?;
    let board = std::fs::read_to_string(&pcb)?;
    let guide = build_guide_with(&circuit, &board, GuideOptions { include_smd: true })?;

    let kicad = kicad_cli_path();
    let top = kicad
        .as_ref()
        .and_then(|k| render_board_png(&pcb, k, true, false, Quality::High).ok());
    let bottom = kicad
        .as_ref()
        .and_then(|k| render_board_png(&pcb, k, true, true, Quality::High).ok());
    match &top {
        Some((_, w, h)) => eprintln!("render {w}×{h}"),
        None => eprintln!("no kicad-cli — schematic fallback diagram"),
    }
    fn as_png(r: &Option<(Vec<u8>, u32, u32)>) -> Option<BoardPng<'_>> {
        r.as_ref().map(|(png, w, h)| BoardPng {
            png: png.as_slice(),
            width: *w,
            height: *h,
        })
    }
    let html = guide_to_html(&guide, as_png(&top), as_png(&bottom));
    std::fs::write(out.with_extension("html"), html)?;
    let jpeg = |r: &Option<(Vec<u8>, u32, u32)>| r.as_ref().and_then(|(p, _, _)| png_to_jpeg(p));
    std::fs::write(
        out.with_extension("pdf"),
        guide_to_pdf(&guide, jpeg(&top).as_deref(), jpeg(&bottom).as_deref()),
    )?;
    eprintln!("wrote {}.html / .pdf", out.display());
    Ok(())
}
