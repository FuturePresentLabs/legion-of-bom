//! MPN sourcing: turn a *generic* circuit part (a value + footprint, no MPN) into
//! ranked, real manufacturer-part-number **suggestions**. lrr; DESIGN 9.6.
//!
//! A generic passive (`"10k"` in an `R_0805` footprint) has no MPN, so nothing
//! downstream can price or order it. This module builds a distributor query from
//! the part (`"10k resistor 0805"`), runs it against the distributors we have keys
//! for (Mouser keyword search — the working parametric path; LCSC/EasyEDA as a
//! best-effort secondary), and ranks the hits.
//!
//! **Suggest-only.** Per the okm human-verification gate, a part must be
//! human-VERIFIED to pass the gate — so this never mutates the [`Part`] or the
//! parts library. It returns candidates a human confirms; the confirm step (CLI
//! `parts verify` today, a dashboard action later) is what writes an MPN back.
//!
//! **Fails gracefully.** No Mouser key → that source is skipped, not an error;
//! network failures yield no candidates rather than panicking. External-tool
//! stages must degrade, per the repo conventions.

use crate::easyeda;
use crate::model::Part;
use crate::mouser::MouserClient;

/// The kind of component a part is, inferred from its library part / footprint /
/// refdes — enough to phrase a distributor query and score package matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Resistor,
    Capacitor,
    Inductor,
    Diode,
    /// An active/complex part (op-amp, MCU, transistor, connector, …) — searched
    /// by its value/name verbatim rather than a "<value> <noun> <package>" phrase.
    Ic,
    /// Nothing recognisable — search the raw value.
    Other,
}

impl PartKind {
    /// The English noun used in a distributor keyword (`"resistor"`), or `None`
    /// for kinds searched by name alone.
    fn noun(self) -> Option<&'static str> {
        match self {
            PartKind::Resistor => Some("resistor"),
            PartKind::Capacitor => Some("capacitor"),
            PartKind::Inductor => Some("inductor"),
            PartKind::Diode => Some("diode"),
            PartKind::Ic | PartKind::Other => None,
        }
    }
}

/// Classify a part. Prefer the explicit `library_part` (`"Device:R"`), then the
/// footprint library prefix (`"Resistor_SMD:…"`), then the refdes letter. A part
/// that already looks like a real device name (letters, e.g. `"TL072"`) is an IC.
pub fn part_kind(part: &Part) -> PartKind {
    // library_part like "Device:R", "Device:C", "Device:D", "Diode:1N4148".
    if let Some(lib) = &part.library_part {
        let sym = lib.rsplit(':').next().unwrap_or(lib);
        if let Some(k) = kind_from_symbol(sym, lib) {
            return k;
        }
    }
    // Footprint library prefix ("Resistor_SMD:R_0805…", "Capacitor_THT:…").
    if let Some(fp) = &part.footprint {
        let lib = fp.split(':').next().unwrap_or(fp).to_ascii_lowercase();
        if lib.starts_with("resistor") {
            return PartKind::Resistor;
        }
        if lib.starts_with("capacitor") {
            return PartKind::Capacitor;
        }
        if lib.starts_with("inductor") {
            return PartKind::Inductor;
        }
        if lib.starts_with("diode") || lib.starts_with("led") {
            return PartKind::Diode;
        }
    }
    // Refdes letter as a last structural hint (R1, C3, L2, D4, U1).
    match refdes_prefix(&part.refdes.0).as_str() {
        "R" => PartKind::Resistor,
        "C" => PartKind::Capacitor,
        "L" | "FB" => PartKind::Inductor,
        "D" => PartKind::Diode,
        "U" | "Q" | "IC" => PartKind::Ic,
        _ => {
            // A value that reads like a part number (a run of letters) is an IC.
            if has_letter_run(&part.value, 2) {
                PartKind::Ic
            } else {
                PartKind::Other
            }
        }
    }
}

/// Map a bare symbol name (`"R"`, `"C_Small"`, `"D_Schottky"`) to a kind.
fn kind_from_symbol(sym: &str, full: &str) -> Option<PartKind> {
    let s = sym.to_ascii_uppercase();
    let full_lower = full.to_ascii_lowercase();
    if s == "R" || s.starts_with("R_") || full_lower.contains("resistor") {
        return Some(PartKind::Resistor);
    }
    if s == "C" || s.starts_with("C_") || full_lower.contains("capacitor") {
        return Some(PartKind::Capacitor);
    }
    if s == "L" || s.starts_with("L_") || full_lower.contains("inductor") {
        return Some(PartKind::Inductor);
    }
    if s == "D" || s.starts_with("D_") || s.starts_with("LED") || full_lower.contains("diode") {
        return Some(PartKind::Diode);
    }
    None
}

/// A human-readable package hint from a KiCad footprint, for the query and for
/// package-match scoring: `"Resistor_SMD:R_0805_2012Metric"` → `"0805"`,
/// `"Package_SO:SOIC-8_3.9x4.9mm_P1.27mm"` → `"SOIC-8"`, THT axial → `"THT"`.
pub fn package_hint(footprint: &str) -> Option<String> {
    let leaf = footprint.split(':').next_back().unwrap_or(footprint);
    // An imperial SMD size code embedded in the name (0402/0603/0805/1206/…).
    for size in [
        "01005", "0201", "0402", "0603", "0805", "1206", "1210", "2010", "2512",
    ] {
        if leaf.contains(size) {
            return Some(size.to_string());
        }
    }
    // A recognisable IC package family, taken up to its first size/pitch token.
    let upper = leaf.to_ascii_uppercase();
    for fam in [
        "SOIC", "SOP", "TSSOP", "MSOP", "SOT-23", "SOT-223", "TO-92", "TO-220", "DIP", "QFN",
        "TQFP", "LQFP", "DFN",
    ] {
        if upper.contains(fam) {
            // Keep a trailing pin count if present ("SOIC-8", "DIP-16").
            if let Some(idx) = upper.find(fam) {
                let tail = &leaf[idx..];
                let pkg: String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                return Some(pkg);
            }
        }
    }
    // Through-hole axial/radial passives: a size code is meaningless, but "THT"
    // narrows the search to leaded parts.
    if upper.contains("THT") || upper.contains("AXIAL") || upper.contains("RADIAL") {
        return Some("THT".to_string());
    }
    None
}

/// Build the distributor keyword query for a part. Passives get
/// `"<value> <noun> <package>"` (`"10k resistor 0805"`); ICs/named parts are
/// searched by value plus any package (`"TL072 SOIC-8"`). Returns `None` when
/// there's nothing searchable (empty value and no name).
pub fn build_query(part: &Part) -> Option<String> {
    let value = part.value.trim();
    // Nothing to anchor on. A bare noun ("resistor") or package ("0805") with no
    // value is not a useful distributor query, so require a value.
    if value.is_empty() {
        return None;
    }
    let kind = part_kind(part);
    let pkg = part.footprint.as_deref().and_then(package_hint);

    let mut terms: Vec<String> = vec![value.to_string()];
    if let Some(noun) = kind.noun() {
        terms.push(noun.to_string());
    }
    if let Some(pkg) = &pkg {
        // "THT" isn't a good distributor keyword on its own for a value-only
        // passive; a size code / IC package is.
        if pkg != "THT" {
            terms.push(pkg.clone());
        }
    }
    Some(terms.join(" "))
}

/// One ranked MPN suggestion for a generic part. SUGGEST-ONLY: presenting this to
/// a human is the whole contract — nothing here is written back automatically.
#[derive(Debug, Clone, PartialEq)]
pub struct MpnCandidate {
    pub mpn: String,
    pub manufacturer: Option<String>,
    /// A short human description / the package, whichever the source gave.
    pub description: Option<String>,
    pub package: Option<String>,
    pub in_stock: Option<u64>,
    pub unit_price: Option<f64>,
    pub datasheet_url: Option<String>,
    pub image_url: Option<String>,
    /// Which distributor surfaced this (`"mouser"`, `"lcsc"`).
    pub source: &'static str,
    /// LCSC component code, when the source is LCSC (the key for a `jlcpcb` fetch).
    pub lcsc_code: Option<String>,
    /// Ranking score (higher is better) — in-stock, exact package match, and a
    /// real price all lift it. Exposed for transparency/testing.
    pub score: i64,
}

/// The distributor clients available for a suggestion run. Mouser needs a key
/// (absent → skipped); LCSC/EasyEDA is keyless. Build with [`SourcingClients::from_env`].
#[derive(Debug, Clone, Default)]
pub struct SourcingClients {
    pub mouser: Option<MouserClient>,
    /// Whether to consult the keyless LCSC/EasyEDA catalog (default on).
    pub use_lcsc: bool,
}

impl SourcingClients {
    /// Build from the environment: a Mouser client if `MOUSER_API_KEY` is set,
    /// LCSC always on (keyless). Never fails — a missing key just disables that
    /// source, so `suggest_mpns` degrades instead of erroring.
    pub fn from_env() -> Self {
        SourcingClients {
            mouser: MouserClient::from_env().ok(),
            use_lcsc: true,
        }
    }

    /// Whether any source is available to search at all.
    pub fn any(&self) -> bool {
        self.mouser.is_some() || self.use_lcsc
    }
}

/// Suggest ranked MPNs for a part. Runs each available distributor search for the
/// part's [`build_query`], merges + de-duplicates by MPN, and ranks. Returns at
/// most `limit` candidates. Never mutates `part`. Best-effort: a failed source
/// contributes nothing; an empty result is a valid (empty) `Vec`, not an error.
pub fn suggest_mpns(part: &Part, clients: &SourcingClients, limit: usize) -> Vec<MpnCandidate> {
    let Some(query) = build_query(part) else {
        return Vec::new();
    };
    let want_pkg = part.footprint.as_deref().and_then(package_hint);
    suggest_for_query(&query, want_pkg.as_deref(), clients, limit)
}

/// [`suggest_mpns`] for a query built elsewhere.
///
/// An imported BOM has no `Part` to derive a query from — its evidence is a
/// comment and a package column, which [`crate::bom_repair`] turns into a
/// keyword. Same search, same ranking, different starting point.
pub fn suggest_by_keyword(
    query: &str,
    clients: &SourcingClients,
    limit: usize,
) -> Vec<MpnCandidate> {
    suggest_for_query(query, None, clients, limit)
}

fn suggest_for_query(
    query: &str,
    want_pkg: Option<&str>,
    clients: &SourcingClients,
    limit: usize,
) -> Vec<MpnCandidate> {
    let query = query.to_string();

    let mut candidates: Vec<MpnCandidate> = Vec::new();

    // Mouser keyword search — the working parametric path for generic passives.
    if let Some(mouser) = &clients.mouser {
        if let Ok(hits) = mouser.search_keyword(&query, limit.min(50) as u16) {
            for pp in hits {
                candidates.push(MpnCandidate {
                    manufacturer: pp.manufacturer.clone(),
                    description: None,
                    package: None,
                    in_stock: pp.in_stock,
                    unit_price: pp.unit_price_at(1),
                    datasheet_url: pp.datasheet_url.clone(),
                    image_url: pp.image_url.clone(),
                    source: "mouser",
                    lcsc_code: None,
                    score: 0,
                    mpn: pp.mpn,
                });
            }
        }
    }

    // LCSC/EasyEDA catalog — best-effort secondary (strong for named parts, weak
    // for generic passives; see `easyeda::LcscCandidate`).
    if clients.use_lcsc {
        for c in easyeda::search_parts(&query, limit) {
            candidates.push(MpnCandidate {
                manufacturer: c.manufacturer,
                description: c.package.clone(),
                package: c.package,
                in_stock: c.stock,
                unit_price: c.unit_price,
                datasheet_url: None,
                image_url: c.image_url,
                source: "lcsc",
                lcsc_code: c.lcsc_code,
                score: 0,
                mpn: c.mpn,
            });
        }
    }

    dedup_and_rank(candidates, want_pkg, limit)
}

/// De-duplicate by MPN (keeping the richer of two same-MPN hits), score, sort,
/// and truncate to `limit`.
fn dedup_and_rank(
    mut candidates: Vec<MpnCandidate>,
    want_pkg: Option<&str>,
    limit: usize,
) -> Vec<MpnCandidate> {
    // Merge duplicates by MPN — prefer the entry that carries a price, then stock.
    candidates.sort_by(|a, b| a.mpn.cmp(&b.mpn));
    candidates.dedup_by(|dup, keep| {
        if dup.mpn != keep.mpn {
            return false;
        }
        // `keep` is kept; fold in anything it's missing from `dup`.
        keep.unit_price = keep.unit_price.or(dup.unit_price);
        keep.in_stock = keep.in_stock.or(dup.in_stock);
        keep.datasheet_url = keep
            .datasheet_url
            .take()
            .or_else(|| dup.datasheet_url.take());
        keep.image_url = keep.image_url.take().or_else(|| dup.image_url.take());
        keep.package = keep.package.take().or_else(|| dup.package.take());
        keep.lcsc_code = keep.lcsc_code.take().or_else(|| dup.lcsc_code.take());
        true
    });

    for c in &mut candidates {
        c.score = score_candidate(c, want_pkg);
    }
    // Highest score first; break ties by more stock, then cheaper, then MPN for
    // determinism.
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.in_stock.unwrap_or(0).cmp(&a.in_stock.unwrap_or(0)))
            .then(
                a.unit_price
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&b.unit_price.unwrap_or(f64::INFINITY)),
            )
            .then(a.mpn.cmp(&b.mpn))
    });
    candidates.truncate(limit);
    candidates
}

/// Score a candidate: in-stock and an exact package match are the strong signals;
/// having a price and a datasheet are minor lifts. Higher is better.
fn score_candidate(c: &MpnCandidate, want_pkg: Option<&str>) -> i64 {
    let mut score = 0i64;
    match c.in_stock {
        Some(n) if n > 0 => score += 100,
        Some(_) => score -= 20, // known-zero stock is a real negative
        None => {}              // unknown stock — neutral
    }
    if let (Some(want), Some(have)) = (want_pkg, c.package.as_deref()) {
        if package_matches(want, have) {
            score += 60;
        }
    }
    if c.unit_price.is_some() {
        score += 15;
    }
    if c.datasheet_url.is_some() {
        score += 5;
    }
    score
}

/// Whether a wanted package hint matches a candidate's reported package,
/// case-insensitively and tolerant of substring form (`"0805"` in
/// `"0805 SMD"`, `"SOIC-8"` vs `"SOP-8"` treated loosely by the size code).
fn package_matches(want: &str, have: &str) -> bool {
    let w = want.to_ascii_lowercase();
    let h = have.to_ascii_lowercase();
    if h.contains(&w) || w.contains(&h) {
        return true;
    }
    // SOIC-8 / SOP-8 / SO-8 share the trailing pin count; match on that.
    fn pins(s: &str) -> Option<&str> {
        s.rsplit('-')
            .next()
            .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
    }
    matches!((pins(&w), pins(&h)), (Some(a), Some(b)) if a == b && w.contains("so") && h.contains("so"))
}

/// Whether `s` contains a run of at least `n` consecutive ASCII letters — a cheap
/// "looks like a part number, not a bare value" test.
fn has_letter_run(s: &str, n: usize) -> bool {
    let mut run = 0usize;
    for c in s.chars() {
        run = if c.is_ascii_alphabetic() { run + 1 } else { 0 };
        if run >= n {
            return true;
        }
    }
    false
}

/// The leading alphabetic prefix of a refdes (`"R12"` → `"R"`, `"FB1"` → `"FB"`).
fn refdes_prefix(refdes: &str) -> String {
    refdes
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Part;

    fn part(refdes: &str, value: &str, footprint: Option<&str>, lib: Option<&str>) -> Part {
        let mut p = Part::new(refdes, value);
        if let Some(fp) = footprint {
            p = p.with_footprint(fp);
        }
        p.library_part = lib.map(str::to_string);
        p
    }

    #[test]
    fn classifies_by_library_part_then_footprint_then_refdes() {
        assert_eq!(
            part_kind(&part("R1", "10k", None, Some("Device:R"))),
            PartKind::Resistor
        );
        assert_eq!(
            part_kind(&part(
                "C3",
                "100n",
                Some("Capacitor_SMD:C_0805_2012Metric"),
                None
            )),
            PartKind::Capacitor
        );
        // Refdes-only fallback.
        assert_eq!(
            part_kind(&part("L2", "10u", None, None)),
            PartKind::Inductor
        );
        assert_eq!(
            part_kind(&part("D4", "1N4148", None, None)),
            PartKind::Diode
        );
        assert_eq!(part_kind(&part("U1", "TL072", None, None)), PartKind::Ic);
        // A device-looking value with an odd refdes → IC.
        assert_eq!(part_kind(&part("X1", "LM13700", None, None)), PartKind::Ic);
    }

    #[test]
    fn package_hint_maps_footprints() {
        assert_eq!(
            package_hint("Resistor_SMD:R_0805_2012Metric").as_deref(),
            Some("0805")
        );
        assert_eq!(
            package_hint("Capacitor_SMD:C_0402_1005Metric").as_deref(),
            Some("0402")
        );
        assert_eq!(
            package_hint("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm").as_deref(),
            Some("SOIC-8")
        );
        assert_eq!(
            package_hint("Package_DIP:DIP-16_W7.62mm").as_deref(),
            Some("DIP-16")
        );
        assert_eq!(
            package_hint("Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm").as_deref(),
            Some("THT")
        );
        assert_eq!(package_hint("Some:Weird_Thing").as_deref(), None);
    }

    #[test]
    fn builds_passive_and_ic_queries() {
        // 10k 0805 resistor → "10k resistor 0805" (the bead's worked example).
        let r = part(
            "R1",
            "10k",
            Some("Resistor_SMD:R_0805_2012Metric"),
            Some("Device:R"),
        );
        assert_eq!(build_query(&r).as_deref(), Some("10k resistor 0805"));

        let c = part("C1", "100n", Some("Capacitor_SMD:C_0603_1608Metric"), None);
        assert_eq!(build_query(&c).as_deref(), Some("100n capacitor 0603"));

        // THT passive: the size code is dropped (leaded), so just value + noun.
        let rt = part("R2", "4.7k", Some("Resistor_THT:R_Axial_DIN0207"), None);
        assert_eq!(build_query(&rt).as_deref(), Some("4.7k resistor"));

        // IC: value + package, no noun.
        let u = part(
            "U1",
            "TL072",
            Some("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm"),
            None,
        );
        assert_eq!(build_query(&u).as_deref(), Some("TL072 SOIC-8"));

        // Nothing to search.
        assert_eq!(build_query(&part("R9", "", None, None)), None);
    }

    #[test]
    fn ranks_instock_and_package_match_highest() {
        let cands = vec![
            // Out of stock, right package → penalised.
            MpnCandidate {
                mpn: "OOS-0805".into(),
                manufacturer: None,
                description: None,
                package: Some("0805".into()),
                in_stock: Some(0),
                unit_price: Some(0.01),
                datasheet_url: None,
                image_url: None,
                source: "lcsc",
                lcsc_code: None,
                score: 0,
            },
            // In stock, right package, priced → best.
            MpnCandidate {
                mpn: "GOOD-0805".into(),
                manufacturer: None,
                description: None,
                package: Some("0805".into()),
                in_stock: Some(500_000),
                unit_price: Some(0.01),
                datasheet_url: Some("http://d".into()),
                image_url: None,
                source: "mouser",
                lcsc_code: None,
                score: 0,
            },
            // In stock, wrong package → middle.
            MpnCandidate {
                mpn: "STOCK-0603".into(),
                manufacturer: None,
                description: None,
                package: Some("0603".into()),
                in_stock: Some(10_000),
                unit_price: None,
                datasheet_url: None,
                image_url: None,
                source: "mouser",
                lcsc_code: None,
                score: 0,
            },
        ];
        let ranked = dedup_and_rank(cands, Some("0805"), 10);
        assert_eq!(ranked[0].mpn, "GOOD-0805");
        assert_eq!(ranked[1].mpn, "STOCK-0603");
        assert_eq!(ranked[2].mpn, "OOS-0805");
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn dedup_merges_same_mpn_across_sources() {
        let cands = vec![
            // Mouser: has datasheet + price, no package/stock.
            MpnCandidate {
                mpn: "RC0805FR-0710KL".into(),
                manufacturer: Some("YAGEO".into()),
                description: None,
                package: None,
                in_stock: None,
                unit_price: Some(0.10),
                datasheet_url: Some("http://ds".into()),
                image_url: None,
                source: "mouser",
                lcsc_code: None,
                score: 0,
            },
            // LCSC: has package + stock + lcsc code, no datasheet.
            MpnCandidate {
                mpn: "RC0805FR-0710KL".into(),
                manufacturer: Some("YAGEO".into()),
                description: Some("0805".into()),
                package: Some("0805".into()),
                in_stock: Some(2_535_100),
                unit_price: None,
                datasheet_url: None,
                image_url: Some("http://img".into()),
                source: "lcsc",
                lcsc_code: Some("C84376".into()),
                score: 0,
            },
        ];
        let ranked = dedup_and_rank(cands, Some("0805"), 10);
        assert_eq!(ranked.len(), 1);
        let m = &ranked[0];
        // The merged candidate carries the union: price (Mouser) + stock/package/
        // lcsc code (LCSC) + datasheet (Mouser).
        assert_eq!(m.unit_price, Some(0.10));
        assert_eq!(m.in_stock, Some(2_535_100));
        assert_eq!(m.package.as_deref(), Some("0805"));
        assert_eq!(m.lcsc_code.as_deref(), Some("C84376"));
        assert_eq!(m.datasheet_url.as_deref(), Some("http://ds"));
    }

    #[test]
    fn package_matches_loosely() {
        assert!(package_matches("0805", "0805"));
        assert!(package_matches("0805", "0805 SMD"));
        assert!(package_matches("SOIC-8", "SOP-8")); // shared pin count, both SO
        assert!(!package_matches("0805", "0603"));
        assert!(!package_matches("SOIC-8", "SOIC-16"));
    }

    #[test]
    fn empty_when_no_sources_or_no_query() {
        // No sources at all → nothing to search (and no network hit).
        let no_sources = SourcingClients {
            mouser: None,
            use_lcsc: false,
        };
        assert!(!no_sources.any());
        let r = part("R1", "10k", Some("Resistor_SMD:R_0805_2012Metric"), None);
        assert!(suggest_mpns(&r, &no_sources, 5).is_empty());

        // No buildable query → empty even with LCSC "on" (short-circuits before any
        // network call, so this stays hermetic).
        let lcsc_on = SourcingClients {
            mouser: None,
            use_lcsc: true,
        };
        let valueless = part("R9", "", None, None);
        assert!(build_query(&valueless).is_none());
        assert!(suggest_mpns(&valueless, &lcsc_on, 5).is_empty());
    }
}
