//! BOM generation from the Circuit model, with optional live pricing.
//! DESIGN.md 9.1; zya.1.
//!
//! Parts are grouped by (mpn, value, footprint); each group is one BOM line with
//! a quantity, its reference designators, and — once priced against a distributor
//! (Mouser, [`crate::mouser`]) — a unit price and extended cost. Output is
//! deterministically ordered. Pricing is applied separately so BOM generation
//! itself stays offline.

use std::collections::BTreeMap;

use crate::package;
use crate::source::CircuitSource;
use crate::theme::{self, esc};

/// What a BOM line is: a placed component, or loose hardware that ships with one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineKind {
    /// A part with pads, placed on the board.
    #[default]
    Component,
    /// A nut, washer or screw that arrives with a part, holds it to the panel,
    /// and appears in no netlist ([`crate::hardware`]). The builder needs it in
    /// the kit and on the sorting sheet; the fab BOM must not see it, because no
    /// pick-and-place machine fits a nut.
    Hardware,
}

/// One line of a BOM: a group of identical parts.
#[derive(Debug, Clone, PartialEq)]
pub struct BomLine {
    /// Component or loose hardware. Defaults to [`LineKind::Component`].
    pub kind: LineKind,
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
                kind: LineKind::Component,
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

    /// The lines a machine places — everything except loose hardware. What the
    /// fab BOM and the CPL are built from.
    pub fn components(&self) -> impl Iterator<Item = &BomLine> {
        self.lines.iter().filter(|l| l.kind == LineKind::Component)
    }

    /// Append the loose hardware the placed parts arrive with — a nut per jack, a
    /// nut and washer per pot ([`crate::hardware`]) — as [`LineKind::Hardware`]
    /// lines carrying the reference designators they serve.
    ///
    /// Idempotent: calling it twice does not double the nuts.
    ///
    /// This is the only place the kit's part count stops being a lie. A netlist
    /// cannot know about a fastener, so without this the sorting sheet is short
    /// by exactly the parts that stop the panel going on.
    pub fn with_hardware(mut self) -> Self {
        self.lines.retain(|l| l.kind != LineKind::Hardware);
        // name -> the refdes it serves, in first-seen order.
        let mut order: Vec<&'static str> = Vec::new();
        let mut serves: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        for line in self.components() {
            let Some(fp) = line.footprint.as_deref() else {
                continue;
            };
            for item in crate::hardware::for_footprint(fp) {
                if !serves.contains_key(item.name) {
                    order.push(item.name);
                }
                serves
                    .entry(item.name)
                    .or_default()
                    .extend(line.refdes.iter().cloned());
            }
        }
        for name in order {
            let mut refdes = serves.remove(name).unwrap_or_default();
            refdes.sort();
            self.lines.push(BomLine {
                kind: LineKind::Hardware,
                mpn: None,
                value: name.to_string(),
                footprint: None,
                refdes,
                unit_price: None,
                ext_price: None,
                image_url: None,
            });
        }
        self
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
    /// data, laid out for a human *sorting physical parts by hand*. Sheet 1 is the
    /// sorting sheet — one cell **per physical part** (quantity repeated), each big
    /// enough to lay the real component on; the last sheet is the BOM list (a
    /// procurement reference).
    ///
    /// Everything is sized to the shared print box ([`theme::CONTENT_W_MM`]), so
    /// the sheet prints true at 100% on Letter or A4 — which matters here more
    /// than anywhere: the resistor colour bands and the package silhouettes are
    /// drawn life-size, and a shrink-to-fit print turns them into a lying ruler.
    ///
    /// `thumbnails[i]` is a pre-fetched, embeddable image (a `data:` URI) for
    /// `lines[i]`, or `None`; a photoless line falls back to a life-size resistor
    /// swatch (THT resistors), then to a life-size package silhouette read from
    /// the footprint, then to a labelled empty slot. Self-contained HTML (inline
    /// CSS). I/O (fetching photos) is the caller's job, keeping this pure.
    pub fn to_visual_html(&self, name: &str, thumbnails: &[Option<String>]) -> String {
        // Sheet 1 — sorting sheet: a cell per physical part, quantity repeated.
        let mut cells = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            let thumb = thumbnails.get(i).and_then(Option::as_ref);
            let qty = line.qty();
            for (k, refdes) in line.refdes.iter().enumerate() {
                cells.push_str(&sorting_cell(line, refdes, thumb, k + 1, qty));
            }
        }

        // Final sheet — the BOM list (procurement reference). Columns that are
        // empty for every line are dropped rather than printed as a wall of "—":
        // an unpriced board used to spend three of seven columns saying nothing.
        let has_mpn = self.lines.iter().any(|l| l.mpn.is_some());
        let has_price = self.lines.iter().any(|l| l.unit_price.is_some());
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
                "<tr class=\"{rowcls}\"><td class=\"c-chk\"><span class=\"chk\"></span></td>\
                 <td class=\"pc\">{cell}</td><td class=\"q mono\">{}×</td>\
                 <td class=\"v\">{}</td><td class=\"pk mono\">{}</td>\
                 <td class=\"r mono\">{}</td>{mpn}{price}</tr>",
                line.qty(),
                esc(&line.value),
                esc(&package_label(line)),
                esc(&line.refdes.join("  ")),
                rowcls = if line.kind == LineKind::Hardware {
                    "hw"
                } else {
                    ""
                },
                mpn = if has_mpn {
                    format!(
                        "<td class=\"m mono\">{}</td>",
                        esc(line.mpn.as_deref().unwrap_or("—"))
                    )
                } else {
                    String::new()
                },
                price = if has_price {
                    format!(
                        "<td class=\"u mono\">{}</td><td class=\"e mono\">{}</td>",
                        money(line.unit_price),
                        money(line.ext_price)
                    )
                } else {
                    String::new()
                },
            ));
        }
        let total = self
            .total()
            .map(|t| format!("<p class=\"total\">Total <b>${t:.2}</b></p>"))
            .unwrap_or_default();

        let parts = self.component_count();
        let sheet_head = theme::masthead(
            "Component sorting sheet",
            name,
            "Print at 100% — “Actual size”, not “Fit to page”. Lay each component on its \
             cell; the resistor bands and package outlines are drawn life-size.",
            &[
                format!("{parts} components"),
                format!("{} distinct parts", self.lines.len()),
            ],
        );
        let list_head = theme::masthead(
            "Bill of materials",
            name,
            "Every line item, grouped.",
            &[format!("{} lines", self.lines.len())],
        );
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">\
             <title>Component sorting sheet — {title}</title>\
             <style>{BASE}{VBOM_CSS}</style></head><body><div class=\"wrap\">\
             <section class=\"sheet\">{sheet_head}<div class=\"grid\">{cells}</div>{foot1}</section>\
             <section class=\"sheet\">{list_head}{total}\
             <table class=\"ptab\"><thead><tr><th class=\"h-chk\"></th><th></th><th>Qty</th>\
             <th>Value</th><th>Package</th><th>Reference designators</th>{mpn_h}{price_h}</tr>\
             </thead><tbody>{rows}</tbody></table>{foot2}</section>\
             </div></body></html>",
            title = esc(name),
            BASE = theme::BASE_CSS,
            mpn_h = if has_mpn { "<th>MPN</th>" } else { "" },
            price_h = if has_price {
                "<th class=\"ra\">Unit</th><th class=\"ra\">Ext</th>"
            } else {
                ""
            },
            foot1 = theme::page_footer(
                &format!("{name} · component sorting sheet"),
                &format!("{parts} components — lay each part on its cell"),
            ),
            foot2 = theme::page_footer(&format!("{name} · bill of materials"), "Last sheet"),
        )
    }
}

/// One sorting-sheet cell: a tick box and this unit's reference designator, a
/// life-size picture (photo / THT-resistor swatch / package silhouette / empty
/// slot), and the value with its package.
///
/// The refdes leads because that is what the cell *is* — one physical component,
/// not a line item — and `k of qty` lets a builder see at a glance that they've
/// found all three of something without recounting the grid.
fn sorting_cell(
    line: &BomLine,
    refdes: &str,
    thumb: Option<&String>,
    k: usize,
    qty: usize,
) -> String {
    let img = match thumb {
        Some(uri) => format!("<img class=\"ph-life\" src=\"{}\" alt=\"\">", esc(uri)),
        None => resistor_lifesize(line)
            .or_else(|| line.footprint.as_deref().and_then(package::silhouette_svg))
            .unwrap_or_else(|| "<div class=\"noph-life\"></div>".into()),
    };
    let of = if qty > 1 {
        format!("<span class=\"of\">{k} of {qty}</span>")
    } else {
        String::new()
    };
    format!(
        "<div class=\"cell grid-cell{hw}\"><div class=\"hd\"><span class=\"chk\"></span>\
         <b class=\"r\">{ref_}</b>{of}</div><div class=\"art\">{img}</div>\
         <div class=\"lbl\"><b class=\"v\">{val}</b><span class=\"pk\">{pkg}</span></div></div>",
        hw = if line.kind == LineKind::Hardware {
            " hw"
        } else {
            ""
        },
        ref_ = esc(refdes),
        val = esc(&line.value),
        pkg = esc(&package_label(line)),
    )
}

/// The short package name for a BOM line. Loose hardware has no footprint and
/// never will, so it says what it is rather than showing an empty column.
fn package_label(line: &BomLine) -> String {
    if line.kind == LineKind::Hardware {
        return "in the bag".into();
    }
    line.footprint
        .as_deref()
        .map(package::short_name)
        .unwrap_or_else(|| "—".into())
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

/// Visual-BOM-specific CSS, layered after [`theme::BASE_CSS`].
///
/// The sorting grid is sized in millimetres against the shared print box: four
/// 43.5 mm cells and three 4 mm gutters exactly span the 186 mm content width, so
/// the sheet uses its full measure instead of leaving a column's worth of margin
/// on the right. Each cell is a fixed height, which keeps the grid on a rhythm a
/// builder can count along, and clips nothing — long values wrap rather than
/// running out past the cell border.
const VBOM_CSS: &str = "\
.sheet{display:flex;flex-direction:column;min-height:255mm;padding-bottom:2mm}\
.sheet+.sheet{border-top:1px dashed var(--hair);margin-top:6mm;padding-top:6mm}\
.total{margin:0 0 2mm;font-size:9pt}\
.total b{font-family:ui-monospace,Menlo,monospace;font-size:12pt}\
.grid{display:grid;grid-template-columns:repeat(4,43.5mm);gap:4mm;margin-top:2mm;\
justify-content:space-between;align-content:start}\
.cell{display:flex;flex-direction:column;height:50mm;padding:2mm;border:.8pt solid var(--ink);\
break-inside:avoid;page-break-inside:avoid;overflow:hidden}\
.cell .hd{display:flex;align-items:center;gap:1.6mm;flex:none;border-bottom:.4pt solid var(--hair);\
padding-bottom:1mm}\
.cell .hd .r{font-family:ui-monospace,'SF Mono',Menlo,monospace;font-size:11pt;font-weight:700}\
.cell .hd .of{margin-left:auto;font-family:ui-monospace,Menlo,monospace;font-size:7pt;\
color:var(--muted);white-space:nowrap}\
.cell .art{flex:1 1 auto;display:flex;align-items:center;justify-content:center;\
flex-direction:column;gap:.8mm;min-height:0;padding:1mm 0}\
.cell .ph-life{max-width:37mm;max-height:31mm;object-fit:contain;display:block}\
.cell .rband-life,.cell .sil{display:block;flex:none}\
.cell .sil-dim{font-family:ui-monospace,Menlo,monospace;font-size:6.5pt;color:var(--muted)}\
.cell .noph-life{width:26mm;height:16mm;border:.3mm dashed var(--hair)}\
.cell .lbl{flex:none;text-align:center;line-height:1.15}\
.cell .lbl .v{display:block;font-weight:700;font-size:9pt;overflow-wrap:anywhere;\
max-height:2.4em;overflow:hidden}\
.cell .lbl .pk{font-family:ui-monospace,Menlo,monospace;font-size:7pt;color:var(--muted)}\
.cell.hw{border-style:dashed}\
.ptab{border-collapse:collapse;width:100%;font-size:9pt}\
.ptab th{text-align:left;font-weight:700;font-size:7.5pt;text-transform:uppercase;\
border-top:.8pt solid var(--ink);border-bottom:.8pt solid var(--ink);padding:.9mm 1.5mm;\
white-space:nowrap}\
.ptab td{border-bottom:.4pt solid var(--hair);padding:1mm 1.5mm;vertical-align:middle}\
.ptab tbody tr:last-child td{border-bottom:.8pt solid var(--ink)}\
.ptab tr>*:first-child{padding-left:0}.ptab tr>*:last-child{padding-right:0}\
.h-chk,.c-chk{width:8mm}\
.pc{width:15mm}img.ph{width:13mm;height:13mm;object-fit:contain;border:.4pt solid var(--hair);\
display:block;background:#fff}\
.noph{display:block;width:13mm;height:13mm;border:.3mm dashed var(--hair)}\
.sw svg{width:58px;height:18px;display:block}\
td.q{width:10mm}td.v{font-weight:700;font-size:10pt}\
td.pk{font-size:8.5pt;white-space:nowrap}\
td.r{font-size:9.5pt;font-weight:700}\
td.m{font-size:8pt}\
tr.hw td{font-style:italic}\
th.ra,td.u,td.e{text-align:right}\
@media print{.sheet{break-after:page;min-height:235mm;border:none;margin:0;padding:0}\
.sheet:last-child{break-after:auto}.sheet+.sheet{border-top:none;margin-top:0;padding-top:0}}";

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
                    kind: LineKind::Component,
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
                    kind: LineKind::Component,
                    mpn: None,
                    value: "4.7k".into(),
                    footprint: Some("Resistor_THT:R_Axial_DIN0207".into()),
                    refdes: vec!["R1".into(), "R2".into()],
                    unit_price: None,
                    ext_price: None,
                    image_url: None,
                },
                // SMD cap, no photo, not a resistor → life-size package outline.
                BomLine {
                    kind: LineKind::Component,
                    mpn: None,
                    value: "159n".into(),
                    footprint: Some("Capacitor_SMD:C_0805_2012Metric".into()),
                    refdes: vec!["C1".into()],
                    unit_price: None,
                    ext_price: None,
                    image_url: None,
                },
                // No footprint at all → nothing honest to draw, so a blank slot.
                BomLine {
                    kind: LineKind::Component,
                    mpn: None,
                    value: "SPDT".into(),
                    footprint: None,
                    refdes: vec!["SW1".into()],
                    unit_price: None,
                    ext_price: None,
                    image_url: None,
                },
            ],
        };
        let thumbs = vec![
            Some("data:image/jpeg;base64,AAAA".to_string()),
            None,
            None,
            None,
        ];
        let html = bom.to_visual_html("demo", &thumbs);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("src=\"data:image/jpeg;base64,AAAA\"")); // photo embedded

        // Sorting sheet: one cell per physical part (qty repeated).
        assert_eq!(html.matches("class=\"cell grid-cell\"").count(), 5);
        assert_eq!(html.matches("class=\"ph-life\"").count(), 1); // IC photo
        assert_eq!(html.matches("class=\"rband-life\"").count(), 2); // both resistors, life-size
        assert!(html.contains("mm\"")); // life-size uses real mm units
                                        // The chip cap falls back to a life-size outline with its measured size —
                                        // enough to tell an 0805 from a 1206 by laying the part on the page.
        assert!(html.contains("class=\"sil\""));
        assert!(html.contains("2 × 1.2 mm"));
        // Only the footprint-less part is left blank; an invented outline would
        // be worse than an honest gap.
        assert_eq!(html.matches("class=\"noph-life\"").count(), 1);

        // Each cell counts off within its group, so "did I find all three?" is
        // answerable without recounting the grid.
        assert!(html.contains(">1 of 2<") && html.contains(">2 of 2<"));

        // The list sheet carries the compact swatch/blank and its own page.
        assert!(html.contains("break-after:page"));
        assert!(
            html.contains("class=\"sw\"") && html.contains("<title>yellow violet red gold</title>")
        );
        assert!(html.contains("class=\"noph\""));
        // Package is a column you sort by, so the list carries it.
        assert!(html.contains(">Package<") && html.contains(">0805<"));
    }

    #[test]
    fn the_bom_list_drops_columns_that_are_empty_for_every_line() {
        // An unpriced, MPN-less board used to print three columns of "—". The
        // header is the tell: no MPN column at all rather than an empty one.
        let mut bom = generate_bom(&circuit());
        for line in &mut bom.lines {
            line.mpn = None;
        }
        let thumbs = vec![None; bom.lines.len()];
        let plain = bom.to_visual_html("demo", &thumbs);
        assert!(!plain.contains(">MPN<"), "no MPNs → no MPN column");
        assert!(
            !plain.contains(">Unit<"),
            "nothing priced → no price columns"
        );

        // Earn the columns back and they reappear.
        let mut full = bom.clone();
        full.lines[0].set_unit_price(0.05);
        full.lines[0].mpn = Some("RC0805FR-071KL".into());
        let html = full.to_visual_html("demo", &thumbs);
        assert!(html.contains(">Unit<") && html.contains(">Ext<"));
        assert!(html.contains(">MPN<"));
    }

    /// A netlist has no line for a nut, so the kit was short by exactly the
    /// parts that stop the panel going on.
    #[test]
    fn hardware_is_derived_from_the_parts_that_need_it() {
        let bom = Bom {
            lines: vec![
                BomLine {
                    kind: LineKind::Component,
                    mpn: None,
                    value: "AudioJack2_SwitchT".into(),
                    footprint: Some(
                        "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical".into(),
                    ),
                    refdes: vec!["J1".into(), "J2".into(), "J4".into()],
                    unit_price: None,
                    ext_price: None,
                    image_url: None,
                },
                BomLine {
                    kind: LineKind::Component,
                    mpn: None,
                    value: "100k".into(),
                    footprint: Some("Potentiometer_THT:Alpha_RD901F".into()),
                    refdes: vec!["RV1".into(), "RV2".into()],
                    unit_price: None,
                    ext_price: None,
                    image_url: None,
                },
                BomLine {
                    kind: LineKind::Component,
                    mpn: None,
                    value: "47k".into(),
                    footprint: Some("Resistor_SMD:R_0603_1608Metric".into()),
                    refdes: vec!["R3".into()],
                    unit_price: None,
                    ext_price: None,
                    image_url: None,
                },
            ],
        };
        let bom = bom.with_hardware();
        let hw: Vec<&BomLine> = bom
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Hardware)
            .collect();
        let by = |n: &str| hw.iter().find(|l| l.value.contains(n)).unwrap();
        // Three jacks, three nuts and three washers — carrying the refdes they
        // serve, so the builder can see what each one is for.
        assert_eq!(by("jack nut").qty(), 3);
        assert_eq!(by("jack nut").refdes, ["J1", "J2", "J4"]);
        assert_eq!(by("Jack washer").qty(), 3);
        // Two pots, two nuts and two washers — a pot's thread is not a jack's.
        assert_eq!(by("pot nut").qty(), 2);
        assert_eq!(by("pot nut").refdes, ["RV1", "RV2"]);
        assert_eq!(by("Pot washer").qty(), 2);
        // The resistor contributes nothing.
        assert_eq!(hw.len(), 4);
        // Running it again must not double the nuts.
        let twice = bom.clone().with_hardware();
        assert_eq!(
            twice
                .lines
                .iter()
                .filter(|l| l.kind == LineKind::Hardware)
                .count(),
            4
        );
    }

    /// Hardware belongs in the kit, not in the fab package.
    #[test]
    fn the_fab_bom_never_sees_loose_hardware() {
        let mut bom = generate_bom(&circuit());
        bom.lines.push(BomLine {
            kind: LineKind::Hardware,
            mpn: None,
            value: "M6 jack nut".into(),
            footprint: None,
            refdes: vec!["J1".into()],
            unit_price: None,
            ext_price: None,
            image_url: None,
        });
        let csv = crate::fab::jlc_bom_csv(&bom);
        assert!(
            !csv.contains("jack nut"),
            "no machine places a nut; it must not reach the fab BOM:\n{csv}"
        );
        assert_eq!(bom.components().count(), bom.lines.len() - 1);
    }

    #[test]
    fn csv_has_header_and_rows() {
        let bom = generate_bom(&circuit());
        let csv = bom.to_csv();
        assert!(csv.starts_with("refdes,qty,mpn,value,footprint,unit_price,ext_price\n"));
        assert_eq!(csv.lines().count(), 1 + bom.lines.len());
    }
}
