//! BOM generation from the Circuit model — first slice of the BOM pipeline.
//! DESIGN.md 9.1.
//!
//! Parts are grouped by (value, footprint); each group is one BOM line with a
//! quantity and its reference designators. Output is deterministically ordered.

use std::collections::BTreeMap;

use crate::source::CircuitSource;

/// One line of a BOM: a group of identical parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BomLine {
    pub value: String,
    pub footprint: Option<String>,
    /// Reference designators in this group (sorted).
    pub refdes: Vec<String>,
}

impl BomLine {
    pub fn qty(&self) -> usize {
        self.refdes.len()
    }
}

/// A complete BOM.
#[derive(Debug, Clone, Default)]
pub struct Bom {
    pub lines: Vec<BomLine>,
}

/// Group a circuit's parts into a BOM by (value, footprint).
pub fn generate_bom(circuit: &dyn CircuitSource) -> Bom {
    let mut groups: BTreeMap<(String, Option<String>), Vec<String>> = BTreeMap::new();
    for part in circuit.parts() {
        groups
            .entry((part.value.clone(), part.footprint.clone()))
            .or_default()
            .push(part.refdes.0.clone());
    }

    let mut lines: Vec<BomLine> = groups
        .into_iter()
        .map(|((value, footprint), mut refdes)| {
            refdes.sort();
            BomLine {
                value,
                footprint,
                refdes,
            }
        })
        .collect();
    // Stable ordering by the first refdes in each group.
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

    /// CSV rendering: `refdes,qty,value,footprint`.
    pub fn to_csv(&self) -> String {
        let mut out = String::from("refdes,qty,value,footprint\n");
        for line in &self.lines {
            out.push_str(&format!(
                "{},{},{},{}\n",
                line.refdes.join(" "),
                line.qty(),
                line.value,
                line.footprint.as_deref().unwrap_or("")
            ));
        }
        out
    }

    /// Aligned plain-text table for the terminal.
    pub fn to_table(&self) -> String {
        let header = ("Refdes", "Qty", "Value", "Footprint");
        let refdes: Vec<String> = self.lines.iter().map(|l| l.refdes.join(" ")).collect();

        let w_ref = refdes
            .iter()
            .map(String::len)
            .chain([header.0.len()])
            .max()
            .unwrap_or(0);
        let w_val = self
            .lines
            .iter()
            .map(|l| l.value.len())
            .chain([header.2.len()])
            .max()
            .unwrap_or(0);

        let mut out = format!(
            "{:<w_ref$}  {:>3}  {:<w_val$}  {}\n",
            header.0, header.1, header.2, header.3
        );
        for (line, refs) in self.lines.iter().zip(&refdes) {
            out.push_str(&format!(
                "{:<w_ref$}  {:>3}  {:<w_val$}  {}\n",
                refs,
                line.qty(),
                line.value,
                line.footprint.as_deref().unwrap_or("-"),
            ));
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
                Part::new("U1", "TL072"), // no footprint
            ],
            nets: vec![],
        }
    }

    #[test]
    fn groups_identical_parts() {
        let bom = generate_bom(&circuit());
        // R1+R2 collapse to one line (qty 2); C1 and U1 are their own lines.
        assert_eq!(bom.lines.len(), 3);
        assert_eq!(bom.component_count(), 4);

        let resistors = bom.lines.iter().find(|l| l.value == "1k").unwrap();
        assert_eq!(resistors.qty(), 2);
        assert_eq!(resistors.refdes, vec!["R1", "R2"]);
    }

    #[test]
    fn flags_missing_footprints() {
        let bom = generate_bom(&circuit());
        assert_eq!(bom.parts_without_footprint(), vec!["U1"]);
    }

    #[test]
    fn csv_has_header_and_rows() {
        let bom = generate_bom(&circuit());
        let csv = bom.to_csv();
        assert!(csv.starts_with("refdes,qty,value,footprint\n"));
        assert_eq!(csv.lines().count(), 1 + bom.lines.len());
    }
}
