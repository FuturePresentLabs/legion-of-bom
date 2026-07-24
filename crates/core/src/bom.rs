//! BOM generation from the Circuit model, with optional live pricing.
//! DESIGN.md 9.1; zya.1.
//!
//! Parts are grouped by (mpn, value, footprint); each group is one BOM line with
//! a quantity, its reference designators, and — once priced against a distributor
//! (Mouser, [`crate::mouser`]) — a unit price and extended cost. Output is
//! deterministically ordered. Pricing is applied separately so BOM generation
//! itself stays offline.

use std::collections::BTreeMap;

use crate::source::CircuitSource;
use crate::theme::{self, esc};

/// One line of a BOM: a group of identical parts.
#[derive(Debug, Clone, PartialEq)]
pub struct BomLine {
    /// Manufacturer part number, if the parts carry one.
    pub mpn: Option<String>,
    pub value: String,
    pub footprint: Option<String>,
    /// Reference designators in this group (sorted).
    pub refdes: Vec<String>,
    /// Unit price once priced, in the distributor's currency.
    pub unit_price: Option<f64>,
    /// Extended price (unit × qty) once priced.
    pub ext_price: Option<f64>,
    /// Product photo URL (from Mouser pricing) for the Visual BOM, if known.
    pub image_url: Option<String>,
}

impl BomLine {
    pub fn qty(&self) -> usize {
        self.refdes.len()
    }

    /// Set the unit price and (re)compute the extended cost for this line.
    pub fn set_unit_price(&mut self, unit_price: f64) {
        self.unit_price = Some(unit_price);
        self.ext_price = Some(unit_price * self.qty() as f64);
    }
}

/// A complete BOM.
#[derive(Debug, Clone, Default)]
pub struct Bom {
    pub lines: Vec<BomLine>,
}

/// Group a circuit's parts into a BOM by (mpn, value, footprint).
pub fn generate_bom(circuit: &dyn CircuitSource) -> Bom {
    let mut groups: BTreeMap<(Option<String>, String, Option<String>), Vec<String>> =
        BTreeMap::new();
    for part in circuit.parts() {
        groups
            .entry((part.mpn.clone(), part.value.clone(), part.footprint.clone()))
            .or_default()
            .push(part.refdes.0.clone());
    }

    let mut lines: Vec<BomLine> = groups
        .into_iter()
        .map(|((mpn, value, footprint), mut refdes)| {
            refdes.sort();
            BomLine {
                mpn,
                value,
                footprint,
                refdes,
                unit_price: None,
                ext_price: None,
                image_url: None,
            }
        })
        .collect();
    lines.sort_by(|a, b| a.refdes.first().cmp(&b.refdes.first()));

    Bom { lines }
}

impl Bom {
    /// Total number of physical components across all lines.
    pub fn component_count(&self) -> usize {
        self.lines.iter().map(BomLine::qty).sum()
    }

    /// Reference designators of parts with no assigned footprint.
    pub fn parts_without_footprint(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter(|l| l.footprint.is_none())
            .flat_map(|l| l.refdes.clone())
            .collect()
    }

    /// Total extended cost of the priced lines, or `None` if nothing is priced.
    pub fn total(&self) -> Option<f64> {
        let priced: Vec<f64> = self.lines.iter().filter_map(|l| l.ext_price).collect();
        (!priced.is_empty()).then(|| priced.iter().sum())
    }

    /// CSV rendering: `refdes,qty,mpn,value,footprint,unit_price,ext_price`.
    pub fn to_csv(&self) -> String {
        let mut out = String::from("refdes,qty,mpn,value,footprint,unit_price,ext_price\n");
        for line in &self.lines {
            out.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                line.refdes.join(" "),
                line.qty(),
                line.mpn.as_deref().unwrap_or(""),
                line.value,
                line.footprint.as_deref().unwrap_or(""),
                line.unit_price
                    .map(|p| format!("{p:.4}"))
                    .unwrap_or_default(),
                line.ext_price
                    .map(|p| format!("{p:.4}"))
                    .unwrap_or_default(),
            ));
        }
        out
    }

    /// Aligned plain-text table for the terminal (footprint omitted — see CSV).
    pub fn to_table(&self) -> String {
        let money = |p: Option<f64>| p.map(|v| format!("{v:.2}")).unwrap_or_else(|| "-".into());
        let rows: Vec<[String; 6]> = self
            .lines
            .iter()
            .map(|l| {
                [
                    l.refdes.join(" "),
                    l.qty().to_string(),
                    l.value.clone(),
                    l.mpn.clone().unwrap_or_else(|| "-".into()),
                    money(l.unit_price),
                    money(l.ext_price),
                ]
            })
            .collect();

        let headers = ["Refdes", "Qty", "Value", "MPN", "Unit", "Ext"];
        let mut widths = headers.map(str::len);
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.len());
            }
        }
        let fmt_row = |cells: &[String; 6]| {
            cells
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
                .collect::<Vec<_>>()
                .join("  ")
        };

        let mut out = fmt_row(&headers.map(String::from));
        out.push('\n');
        for row in &rows {
            out.push_str(&fmt_row(row));
            out.push('\n');
        }
        out
    }

    /// Render a **Visual BOM / Component Sorting Sheet** (DESIGN 7.6): the same BOM
    /// data, laid out for a human *sorting physical parts by hand*. Page 1 is the
    /// sorting sheet — one cell **per physical part** (quantity repeated), each
    /// shown at ~life-size (through-hole resistors truly life-size via CSS `mm`,
    /// photos generously sized) so a builder prints it and lays each component on
    /// its cell. Page 2 is the compact BOM list (a procurement reference).
    ///
    /// `thumbnails[i]` is a pre-fetched, embeddable image (a `data:` URI) for
    /// `lines[i]`, or `None`; a photoless line falls back to a life-size resistor
    /// swatch (THT resistors) or a labelled empty slot. Self-contained HTML (inline
    /// CSS). I/O (fetching photos) is the caller's job, keeping this pure.
    pub fn to_visual_html(&self, name: &str, thumbnails: &[Option<String>]) -> String {
        // Page 1 — sorting sheet: a cell per physical part, quantity repeated.
        let mut cells = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            let thumb = thumbnails.get(i).and_then(Option::as_ref);
            for refdes in &line.refdes {
                cells.push_str(&sorting_cell(line, refdes, thumb));
            }
        }

        // Page 2 — the compact BOM list (procurement reference).
        let mut rows = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            let thumb = thumbnails.get(i).and_then(Option::as_ref);
            let cell = match thumb {
                Some(uri) => format!("<img class=\"ph\" src=\"{}\" alt=\"\">", esc(uri)),
                None => {
                    resistor_swatch(line).unwrap_or_else(|| "<span class=\"noph\"></span>".into())
                }
            };
            let money =
                |p: Option<f64>| p.map(|v| format!("${v:.2}")).unwrap_or_else(|| "—".into());
            rows.push_str(&format!(
                "<tr><td class=\"pc\">{cell}</td><td class=\"q\">{}</td><td class=\"v\">{}</td>\
                 <td class=\"m\">{}</td><td class=\"r\">{}</td><td class=\"u\">{}</td>\
                 <td class=\"e\">{}</td></tr>",
                line.qty(),
                esc(&line.value),
                esc(line.mpn.as_deref().unwrap_or("—")),
                esc(&line.refdes.join(", ")),
                money(line.unit_price),
                money(line.ext_price),
            ));
        }
        let total = self
            .total()
            .map(|t| format!("<p class=\"total\">Total: <b>${t:.2}</b></p>"))
            .unwrap_or_default();

        let parts = self.component_count();
        let sheet_head = theme::masthead(
            "Component sorting sheet",
            name,
            "Print at 100% (no scaling) and lay each part on its cell. Resistors are drawn \
             life-size — match the color bands.",
            &[
                format!("{parts} components"),
                format!("{} lines", self.lines.len()),
            ],
        );
        let list_head = theme::masthead(
            "Bill of materials",
            name,
            "Every line item, grouped and priced.",
            &[],
        );
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">\
             <title>Component sorting sheet — {title}</title>\
             <style>{BASE}{VBOM_CSS}</style></head><body>\
             <section class=\"page sheet\"><div class=\"wrap\">{sheet_head}\
             <div class=\"grid\">{cells}</div></div></section>\
             <section class=\"page list\"><div class=\"wrap\">{list_head}{total}\
             <table><thead><tr><th></th><th>Qty</th><th>Value</th><th>MPN</th><th>Refs</th>\
             <th>Unit</th><th>Ext</th></tr></thead><tbody>{rows}</tbody></table></div></section>\
             </body></html>",
            title = esc(name),
            BASE = theme::BASE_CSS,
        )
    }
}

/// One sorting-sheet cell: a life-size image (photo / THT-resistor swatch / empty
/// slot) plus the value and this unit's reference designator.
fn sorting_cell(line: &BomLine, refdes: &str, thumb: Option<&String>) -> String {
    let img = match thumb {
        Some(uri) => format!("<img class=\"ph-life\" src=\"{}\" alt=\"\">", esc(uri)),
        None => resistor_lifesize(line).unwrap_or_else(|| "<div class=\"noph-life\"></div>".into()),
    };
    format!(
        "<div class=\"cell grid-cell\">{img}<div class=\"lbl\"><b class=\"v\">{}</b>\
         <span class=\"r\">{}</span></div></div>",
        esc(&line.value),
        esc(refdes),
    )
}

/// Whether a BOM line is a through-hole resistor (color bands are a THT thing).
fn is_tht_resistor(line: &BomLine) -> bool {
    let is_resistor = line
        .refdes
        .first()
        .is_some_and(|r| r.starts_with('R') && !r.starts_with("RV"));
    let is_tht = line
        .footprint
        .as_deref()
        .is_some_and(|f| f.to_ascii_uppercase().contains("THT"));
    is_resistor && is_tht
}

/// A through-hole resistor line's compact color-code swatch (for the BOM list), or
/// `None` for anything that isn't a THT resistor with a parseable value.
fn resistor_swatch(line: &BomLine) -> Option<String> {
    is_tht_resistor(line)
        .then(|| crate::resistor::color_code(&line.value))
        .flatten()
        .map(|cc| format!("<span class=\"sw\">{}</span>", cc.to_svg(58.0, 18.0)))
}

/// A through-hole resistor line's **life-size** color-code swatch (sorting sheet).
fn resistor_lifesize(line: &BomLine) -> Option<String> {
    is_tht_resistor(line)
        .then(|| crate::resistor::color_code(&line.value))
        .flatten()
        .map(|cc| cc.to_svg_lifesize())
}

/// Visual-BOM-specific CSS, layered after [`theme::BASE_CSS`]. Data (values, MPN,
/// refs, prices) is set in the shared monospace face; the sorting cells sit on the
/// engineering-grid substrate so each reads as a spot on the bench mat.
const VBOM_CSS: &str = "\
.total{margin:.1rem 0 1rem;font-size:1.02rem}.total b{font-family:ui-monospace,Menlo,monospace}\
.grid{display:flex;flex-wrap:wrap;gap:3.5mm;margin-top:1.3rem;align-items:flex-start}\
.cell{display:flex;flex-direction:column;align-items:center;gap:1.6mm;\
width:38mm;min-height:44mm;padding:2.2mm;border:1px solid var(--line);border-radius:6px;\
break-inside:avoid;page-break-inside:avoid}\
.cell .ph-life{width:30mm;height:30mm;object-fit:contain;display:block;margin-top:auto}\
.cell .rband-life{display:block;margin-top:auto}\
.cell .noph-life{width:24mm;height:16mm;border:1px dashed #c7ccc9;border-radius:3px;margin-top:auto}\
.cell .lbl{margin-top:auto;font-size:.74rem;text-align:center;line-height:1.18}\
.cell .lbl .v{display:block;font-weight:700;font-family:ui-monospace,'SF Mono',Menlo,monospace}\
.cell .lbl .r{color:var(--muted);font-family:ui-monospace,Menlo,monospace;font-variant-numeric:tabular-nums}\
table{border-collapse:collapse;width:100%;font-size:.92rem}\
th{text-align:left;color:var(--muted);font-weight:600;font-size:.72rem;letter-spacing:.06em;\
text-transform:uppercase;border-bottom:1.5px solid var(--copper);padding:.45rem .55rem}\
td{border-bottom:1px solid var(--line);padding:.45rem .55rem;vertical-align:middle}\
.pc{width:60px}img.ph{width:48px;height:48px;object-fit:contain;border:1px solid var(--line);\
border-radius:5px;display:block;background:#fff}\
.noph{display:block;width:48px;height:48px;border:1px dashed #d5d0c5;border-radius:5px}\
.sw svg{width:58px;height:18px;display:block}\
td.q,td.u,td.e,td.v,td.m,td.r{font-family:ui-monospace,'SF Mono',Menlo,monospace}\
td.q,td.u,td.e{font-variant-numeric:tabular-nums}td.e,td.u{text-align:right}\
td.v{font-weight:600}td.m{color:#7a4a22;font-size:.82rem}\
td.r{color:var(--muted);font-size:.82rem}\
@media print{.list{break-before:page}}";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Part};

    fn circuit() -> Circuit {
        Circuit {
            name: "demo".into(),
            parts: vec![
                Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
                Part::new("R2", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
                Part::new("C1", "159n").with_footprint("Capacitor_SMD:C_0805_2012Metric"),
                Part::new("U1", "LM13700").with_mpn("LM13700M/NOPB"), // no footprint
            ],
            nets: vec![],
        }
    }

    #[test]
    fn groups_identical_parts() {
        let bom = generate_bom(&circuit());
        assert_eq!(bom.lines.len(), 3); // R1+R2 collapse; C1 and U1 separate
        assert_eq!(bom.component_count(), 4);

        let resistors = bom.lines.iter().find(|l| l.value == "1k").unwrap();
        assert_eq!(resistors.qty(), 2);
        assert_eq!(resistors.refdes, vec!["R1", "R2"]);

        let opamp = bom.lines.iter().find(|l| l.mpn.is_some()).unwrap();
        assert_eq!(opamp.mpn.as_deref(), Some("LM13700M/NOPB"));
    }

    #[test]
    fn flags_missing_footprints() {
        let bom = generate_bom(&circuit());
        assert_eq!(bom.parts_without_footprint(), vec!["U1"]);
    }

    #[test]
    fn pricing_computes_extended_and_total() {
        let mut bom = generate_bom(&circuit());
        assert_eq!(bom.total(), None); // nothing priced yet
                                       // Price the resistor line (qty 2) at $0.05.
        let resistors = bom.lines.iter_mut().find(|l| l.value == "1k").unwrap();
        resistors.set_unit_price(0.05);
        assert_eq!(resistors.ext_price, Some(0.10));
        assert_eq!(bom.total(), Some(0.10));
    }

    #[test]
    fn visual_bom_falls_back_photo_then_swatch_then_blank() {
        let bom = Bom {
            lines: vec![
                // Priced IC with a fetched photo → embedded <img>.
                BomLine {
                    mpn: Some("LM13700".into()),
                    value: "LM13700".into(),
                    footprint: Some("Package_SO:SOIC-16".into()),
                    refdes: vec!["U1".into()],
                    unit_price: Some(1.07),
                    ext_price: Some(1.07),
                    image_url: Some("https://x/y.jpg".into()),
                },
                // Two THT resistors, no photo → life-size swatch, repeated per unit.
                BomLine {
                    mpn: None,
                    value: "4.7k".into(),
                    footprint: Some("Resistor_THT:R_Axial_DIN0207".into()),
                    refdes: vec!["R1".into(), "R2".into()],
                    unit_price: None,
                    ext_price: None,
                    image_url: None,
                },
                // SMD cap, no photo, not a resistor → blank chip.
                BomLine {
                    mpn: None,
                    value: "159n".into(),
                    footprint: Some("Capacitor_SMD:C_0805".into()),
                    refdes: vec!["C1".into()],
                    unit_price: None,
                    ext_price: None,
                    image_url: None,
                },
            ],
        };
        let thumbs = vec![Some("data:image/jpeg;base64,AAAA".to_string()), None, None];
        let html = bom.to_visual_html("demo", &thumbs);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("src=\"data:image/jpeg;base64,AAAA\"")); // photo embedded

        // Sorting sheet: one cell per physical part (qty repeated) → U1 + R1 + R2 + C1.
        assert_eq!(html.matches("class=\"cell grid-cell\"").count(), 4);
        assert_eq!(html.matches("class=\"ph-life\"").count(), 1); // IC photo
        assert_eq!(html.matches("class=\"rband-life\"").count(), 2); // both resistors, life-size
        assert!(html.contains("mm\"")); // life-size uses real mm units
        assert_eq!(html.matches("class=\"noph-life\"").count(), 1); // empty slot for the cap

        // Page 2 — the BOM list carries the compact swatch/blank and starts on a new page.
        assert!(html.contains("break-before:page"));
        assert!(
            html.contains("class=\"sw\"") && html.contains("<title>yellow violet red gold</title>")
        );
        assert!(html.contains("class=\"noph\""));
    }

    #[test]
    fn csv_has_header_and_rows() {
        let bom = generate_bom(&circuit());
        let csv = bom.to_csv();
        assert!(csv.starts_with("refdes,qty,mpn,value,footprint,unit_price,ext_price\n"));
        assert_eq!(csv.lines().count(), 1 + bom.lines.len());
    }
}
