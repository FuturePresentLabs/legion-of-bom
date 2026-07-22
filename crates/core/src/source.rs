//! The [`CircuitSource`] trait — the boundary every stage reads through.
//!
//! Stages depend on this trait, never on a concrete DSL/netlist type, so that
//! adding a second frontend (native Rust DSL, or the extracted IR) is one new
//! implementation rather than a rewrite. See DESIGN.md 2.3, 3.3.

use crate::model::{Circuit, Net, Part};

/// Read-only access to a circuit for pipeline stages.
pub trait CircuitSource {
    /// Human-readable circuit name.
    fn name(&self) -> &str;
    /// All component instances.
    fn parts(&self) -> &[Part];
    /// All electrical nets.
    fn nets(&self) -> &[Net];
}

/// The in-memory [`Circuit`] is the canonical source; other producers (a SKiDL
/// netlist parser, a future DSL) build one of these.
impl CircuitSource for Circuit {
    fn name(&self) -> &str {
        &self.name
    }

    fn parts(&self) -> &[Part] {
        &self.parts
    }

    fn nets(&self) -> &[Net] {
        &self.nets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Part, RefDes};

    #[test]
    fn circuit_is_a_source() {
        let mut c = Circuit::new("demo");
        c.parts.push(Part::new("R1", "10k"));
        // Consume it only through the trait, the way a stage would.
        fn count_parts(src: &dyn CircuitSource) -> usize {
            src.parts().len()
        }
        assert_eq!(count_parts(&c), 1);
        assert_eq!(c.name(), "demo");
        assert_eq!(c.parts()[0].refdes, RefDes("R1".into()));
    }
}
