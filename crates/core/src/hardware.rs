//! Loose hardware — the nuts and washers that arrive in the bag with a panel
//! part, hold it to the front panel, and appear in no netlist.
//!
//! A jack has pads and a footprint, so it reaches the BOM on its own. Its nut
//! does not: a nut is not a circuit element, so nothing upstream knows it exists.
//! The builder discovers the omission at the point the panel will not go on,
//! which is the worst possible moment — the board is populated and the missing
//! part costs pennies.
//!
//! So it is derived here, from the footprint, and folded into the BOM as lines
//! marked [`LineKind::Hardware`](crate::bom::LineKind::Hardware): present on the
//! sorting sheet and the pull-and-sort list, absent from the fab BOM, because no
//! pick-and-place machine has ever fitted a nut.
//!
//! Deliberately narrow. Each rule below is a part we actually build with and
//! whose hardware we can state exactly; a footprint we don't recognise gets
//! nothing rather than a guess, on the same principle as
//! [`package::body_mm`](crate::package::body_mm) — a BOM that invents a fastener
//! is worse than one that omits it, because the builder will go looking for it.

/// One piece of loose hardware that ships with a part: one per part, per rule.
///
/// There is deliberately no "quantity per part" — a part needing two nuts gets
/// two rules with distinct names ("upper nut", "lower nut"), which is also what
/// a builder counting parts onto a sorting sheet wants to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareItem {
    /// What to look for in the bag.
    pub name: &'static str,
    /// What it does, for the sorting sheet and the guide.
    pub note: &'static str,
}

const JACK_NUT: HardwareItem = HardwareItem {
    name: "M6 jack nut",
    note: "One per jack. Goes on the front of the panel; tighten it before soldering.",
};
const JACK_WASHER: HardwareItem = HardwareItem {
    name: "Jack washer",
    note: "One per jack, between the nut and the panel face.",
};
const POT_NUT: HardwareItem = HardwareItem {
    name: "M7 pot nut",
    note: "One per pot. Tighten before soldering so the pot pulls square to the panel.",
};
const POT_WASHER: HardwareItem = HardwareItem {
    name: "Pot washer",
    note: "One per pot, between the nut and the panel face.",
};

/// Footprint fragments → the hardware that part arrives with. Matched
/// case-insensitively against the whole footprint id, so both the library prefix
/// (`Connector_Audio:`) and the footprint name can carry the evidence.
const RULES: &[(&[&str], &[HardwareItem])] = &[
    (
        &["jack_3.5mm", "pj398sm", "pj301", "thonkiconn", "audiojack"],
        &[JACK_NUT, JACK_WASHER],
    ),
    (
        &["potentiometer", "rd901f", "rk09", "trimmer_pot"],
        &[POT_NUT, POT_WASHER],
    ),
];

/// The loose hardware that ships with the part at `footprint`, or empty when we
/// don't recognise it.
pub fn for_footprint(footprint: &str) -> &'static [HardwareItem] {
    let fp = footprint.to_ascii_lowercase();
    for (fragments, items) in RULES {
        if fragments.iter().any(|f| fp.contains(f)) {
            return items;
        }
    }
    &[]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_jacks_bring_a_nut_and_a_washer() {
        let hw =
            for_footprint("Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical_CircularHoles");
        assert_eq!(hw.len(), 2);
        assert!(hw.iter().any(|h| h.name.contains("jack nut")));
        assert!(hw.iter().any(|h| h.name.contains("washer")));
    }

    #[test]
    fn pots_bring_their_own_nut_and_washer() {
        let hw =
            for_footprint("Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical");
        assert_eq!(hw.len(), 2);
        assert!(hw.iter().any(|h| h.name.contains("pot nut")));
        // A pot's hardware is not a jack's — the threads differ.
        assert!(!hw.iter().any(|h| h.name.contains("jack")));
    }

    #[test]
    fn an_unrecognised_footprint_invents_nothing() {
        // Better to omit a fastener than to send the builder hunting for one
        // that was never in the bag.
        assert!(for_footprint("Resistor_SMD:R_0603_1608Metric").is_empty());
        assert!(for_footprint("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm").is_empty());
        assert!(for_footprint("").is_empty());
    }

    #[test]
    fn matching_is_case_insensitive_and_reads_the_whole_id() {
        // The library prefix can be the only place the evidence lives.
        assert!(!for_footprint("MyLib:THONKICONN_Vertical").is_empty());
        assert!(!for_footprint("Potentiometer_THT:Alpha_RD901F").is_empty());
    }
}
