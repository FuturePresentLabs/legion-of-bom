//! Preview the Visual BOM *layout* (5uj.4) — the `Bom::to_visual_html` render over
//! a representative parts mix, offline: the through-hole resistor color swatches
//! and the graceful blank cells, with no network.
//!
//! Photos are left empty here on purpose (this example does no I/O). The real
//! `lob bom --visual` fills them in from EasyEDA/LCSC ([`legion_of_bom_core::easyeda`])
//! keyed by MPN or distinctive value — see the LM13700 / TL072 photos it embeds.
//!
//! `cargo run -p legion-of-bom-core --example visual_bom > vbom.html`

use legion_of_bom_core::bom::{Bom, BomLine};

fn line(mpn: Option<&str>, value: &str, fp: &str, unit: Option<f64>, refs: &[&str]) -> BomLine {
    let refdes: Vec<String> = refs.iter().map(|s| s.to_string()).collect();
    let ext = unit.map(|u| u * refdes.len() as f64);
    BomLine {
        mpn: mpn.map(str::to_string),
        value: value.to_string(),
        footprint: Some(fp.to_string()),
        refdes,
        unit_price: unit,
        ext_price: ext,
        image_url: None, // no fetchable auto-source today (see module docs)
    }
}

fn main() {
    let bom = Bom {
        lines: vec![
            line(
                Some("LM13700N/NOPB"),
                "LM13700",
                "Package_DIP:DIP-16_W7.62mm",
                Some(2.31),
                &["U1"],
            ),
            line(
                Some("TL072CP"),
                "TL072",
                "Package_DIP:DIP-8_W7.62mm",
                Some(0.68),
                &["U2"],
            ),
            line(
                None,
                "47k",
                "Resistor_THT:R_Axial_DIN0207",
                Some(0.02),
                &["R3", "R5"],
            ),
            line(
                None,
                "51k",
                "Resistor_THT:R_Axial_DIN0207",
                Some(0.02),
                &["R4"],
            ),
            line(
                None,
                "4.7k",
                "Resistor_THT:R_Axial_DIN0207",
                Some(0.02),
                &["R1"],
            ),
            line(
                None,
                "100n",
                "Capacitor_THT:C_Disc_D5.0mm",
                Some(0.05),
                &["C2", "C3"],
            ),
            line(
                None,
                "47n",
                "Capacitor_THT:C_Disc_D5.0mm",
                Some(0.05),
                &["C1"],
            ),
            line(
                None,
                "100k",
                "Potentiometer_THT:Alpha_RD901F",
                Some(0.95),
                &["RV1", "RV2"],
            ),
            line(
                Some("PJ398SM"),
                "AudioJack",
                "Connector_Audio:Jack_3.5mm",
                Some(0.42),
                &["J1", "J2"],
            ),
        ],
    };
    // No thumbnails supplied → every line falls back to a swatch (THT resistors)
    // or a blank chip; exactly what a no-photo-source run produces today.
    let thumbnails = vec![None; bom.lines.len()];
    print!(
        "{}",
        bom.to_visual_html("VC Slew Limiter (demo)", &thumbnails)
    );
}
