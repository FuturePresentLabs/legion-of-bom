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
}

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
    fn csv_has_header_and_rows() {
        let bom = generate_bom(&circuit());
        let csv = bom.to_csv();
        assert!(csv.starts_with("refdes,qty,mpn,value,footprint,unit_price,ext_price\n"));
        assert_eq!(csv.lines().count(), 1 + bom.lines.len());
    }
}
