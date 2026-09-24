//! Fabrication readiness is one explicit contract, not an inference from which
//! files happened to be written.

use serde::{Deserialize, Serialize};

/// Stable machine-readable gate written beside a fabrication package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabReadiness {
    pub schema: String,
    pub ready: bool,
    pub unrouted_connections: Vec<String>,
    pub drc_errors: usize,
    pub drc_unconnected: usize,
    pub unresolved_order_codes: Vec<String>,
    pub blockers: Vec<String>,
}

impl FabReadiness {
    /// Evaluate the three non-negotiable fabrication gates.
    #[must_use]
    pub fn assess(
        mut unrouted_connections: Vec<String>,
        drc_errors: usize,
        drc_unconnected: usize,
        mut unresolved_order_codes: Vec<String>,
    ) -> Self {
        unrouted_connections.sort();
        unrouted_connections.dedup();
        unresolved_order_codes.sort();
        unresolved_order_codes.dedup();

        let mut blockers = Vec::new();
        if !unrouted_connections.is_empty() {
            blockers.push(format!(
                "{} unrouted connection(s)",
                unrouted_connections.len()
            ));
        }
        if drc_errors > 0 {
            blockers.push(format!("{drc_errors} DRC error(s)"));
        }
        if drc_unconnected > 0 {
            blockers.push(format!("{drc_unconnected} DRC unconnected item(s)"));
        }
        if !unresolved_order_codes.is_empty() {
            blockers.push(format!(
                "{} component(s) lack exact order codes",
                unresolved_order_codes.len()
            ));
        }

        Self {
            schema: "legion-of-bom.fab-readiness.v1".into(),
            ready: blockers.is_empty(),
            unrouted_connections,
            drc_errors,
            drc_unconnected,
            unresolved_order_codes,
            blockers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_requires_every_gate_to_be_clean() {
        let ready = FabReadiness::assess(Vec::new(), 0, 0, Vec::new());
        assert!(ready.ready);
        assert!(ready.blockers.is_empty());
    }

    #[test]
    fn every_failure_is_preserved_and_ordered() {
        let report = FabReadiness::assess(
            vec!["N2".into(), "N1".into(), "N1".into()],
            3,
            2,
            vec!["U1".into(), "C2".into()],
        );
        assert!(!report.ready);
        assert_eq!(report.unrouted_connections, ["N1", "N2"]);
        assert_eq!(report.unresolved_order_codes, ["C2", "U1"]);
        assert_eq!(report.blockers.len(), 4);
    }
}
