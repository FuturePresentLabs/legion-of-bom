//! First-party design rules: one source for placement constraints, routing
//! clearances, and the DRC gate.
//!
//! Rules are read from KiCad's *documented formats* so an existing project's
//! intent is honored — `.kicad_dru` custom rules and `.kicad_pro` net classes —
//! and written back to `.kicad_dru` so KiCad's DRC and Lob agree. Only the data
//! is read from KiCad: its DRC *check* code is GPLv3 and is deliberately not
//! copied; the checks are reimplemented from the standards below.
//!
//! Unsupported constraints and conditions are **retained verbatim**, not
//! dropped, so coverage can be reported and an unhandled rule fails loud rather
//! than silently passing a board.
//!
//! @derives-from url:https://dev-docs.kicad.org/en/file-formats/sexpr-pcb/ —
//!   custom-rule and net-settings encoding; data interop only, no code reuse
//! @derives-from url:https://github.com/ziteh/kicad-design-rules — community
//!   rule-file shapes used as cross-checks; no licence, so shapes only, no text

use serde::{Deserialize, Serialize};

use crate::sexpr::Sexpr;

/// A net class: the rule set applied to a named group of nets. Mirrors KiCad's
/// net class fields so import/export round-trips.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetClassRule {
    pub name: String,
    #[serde(default)]
    pub nets: Vec<String>,
    pub clearance_mm: f64,
    pub track_width_mm: f64,
    pub via_dia_mm: f64,
    pub via_drill_mm: f64,
}

/// A constructive constraint kind. `Other` keeps an unrecognized token alive so
/// it can be reported rather than lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    Clearance,
    TrackWidth,
    ViaDiameter,
    ViaDrill,
    HoleSize,
    EdgeClearance,
    AnnularWidth,
    SilkClearance,
    Other,
}

impl ConstraintKind {
    fn from_token(token: &str) -> Self {
        match token {
            "clearance" => Self::Clearance,
            "track_width" => Self::TrackWidth,
            "via_diameter" => Self::ViaDiameter,
            "via_drill" => Self::ViaDrill,
            "hole_size" => Self::HoleSize,
            "edge_clearance" => Self::EdgeClearance,
            "annular_width" => Self::AnnularWidth,
            "silk_clearance" => Self::SilkClearance,
            _ => Self::Other,
        }
    }
}

/// One parsed custom-rule constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomConstraint {
    /// The rule's name, as written.
    pub name: String,
    pub kind: ConstraintKind,
    /// The original kind token, kept so `kind == Other` still round-trips.
    pub kind_raw: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mm: Option<f64>,
    /// The rule's `(condition "…")` expression, verbatim; a checker that cannot
    /// evaluate it must report the rule as unsupported, not skip it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
}

/// The single source of truth for a board's manufacturing rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DesignRules {
    #[serde(default)]
    pub net_classes: Vec<NetClassRule>,
    #[serde(default)]
    pub constraints: Vec<CustomConstraint>,
}

impl DesignRules {
    /// JLCPCB-capable 2-layer defaults as *data* — the one place these numbers
    /// live. `RouteOptions::default` and the emitted `.kicad_dru` both derive
    /// from this, so the router, checker, and KiCad's oracle cannot drift.
    pub fn jlcpcb_two_layer_default() -> Self {
        Self {
            net_classes: vec![NetClassRule {
                name: "Default".into(),
                nets: Vec::new(),
                clearance_mm: 0.2,
                track_width_mm: 0.25,
                via_dia_mm: 0.8,
                via_drill_mm: 0.4,
            }],
            constraints: vec![CustomConstraint {
                name: "copper edge clearance".into(),
                kind: ConstraintKind::EdgeClearance,
                kind_raw: "edge_clearance".into(),
                min_mm: Some(0.5),
                max_mm: None,
                condition: None,
            }],
        }
    }

    /// The class whose `nets` list contains `net`, if any.
    fn class_for(&self, net: &str) -> Option<&NetClassRule> {
        self.net_classes
            .iter()
            .find(|c| c.nets.iter().any(|n| n == net))
    }

    /// The class that governs `net`: the one that lists it, else the default
    /// class. Returns `None` only when no classes are declared at all, so a
    /// caller falls back to its own scalar default rather than guessing zero.
    pub fn net_class(&self, net: &str) -> Option<&NetClassRule> {
        self.class_for(net).or_else(|| self.default_class())
    }

    /// The fallback class: one literally named `Default`, else the first.
    pub fn default_class(&self) -> Option<&NetClassRule> {
        self.net_classes
            .iter()
            .find(|c| c.name == "Default")
            .or_else(|| self.net_classes.first())
    }

    /// Pair clearance between two nets, KiCad semantics: the stricter of the
    /// two classes wins.
    pub fn pair_clearance(&self, a: &str, b: &str) -> f64 {
        let of = |n: &str| {
            self.class_for(n)
                .or_else(|| self.default_class())
                .map(|c| c.clearance_mm)
        };
        match (of(a), of(b)) {
            (Some(x), Some(y)) => x.max(y),
            (Some(x), None) | (None, Some(x)) => x,
            (None, None) => 0.0,
        }
    }

    /// The strictest clearance any class demands. The maze grid paints every
    /// copper halo at this distance: per-pair halos would need owner-aware
    /// exclusion rings, and a route that clears the strictest class clears
    /// every pair. Costs corridor slack to loose classes' nets; never claims
    /// more freedom than the checker will allow.
    pub fn strictest_clearance(&self) -> Option<f64> {
        self.net_classes
            .iter()
            .map(|c| c.clearance_mm)
            .reduce(f64::max)
    }

    /// The widest track any class demands, as the conservative grid width.
    pub fn widest_track(&self) -> Option<f64> {
        self.net_classes
            .iter()
            .map(|c| c.track_width_mm)
            .reduce(f64::max)
    }

    /// Copper-to-board-edge clearance from an `edge_clearance` constraint, if
    /// one is stated.
    pub fn edge_clearance_mm(&self) -> Option<f64> {
        self.constraints
            .iter()
            .find(|c| c.kind == ConstraintKind::EdgeClearance)
            .and_then(|c| c.min_mm)
    }

    /// The conventional width for a supply-rail trace: enough copper that a
    /// rail's voltage drop and fusing current stop being an accident of
    /// whatever the signal width happened to be. Well inside JLCPCB's 2-layer
    /// capability.
    pub const RAIL_WIDTH_MM: f64 = 0.4;

    /// Auto-derive this board's rules from its net names: a `Default` class
    /// mirroring the router's current scalars, plus one `Power` class listing
    /// every supply rail (`is_supply_rail`) when the rails would actually be
    /// wider than the signal default.
    ///
    /// Clearance stays at the board default for the rail class: tightening it
    /// would paint the whole shared grid at the stricter value and cost every
    /// *signal* corridor space it does not need (the asymmetric per-pair grid
    /// that would make class clearance free is legion-of-bom-4s7y). Width is
    /// per-owner, so a wide rail only widens its own exclusion — signals keep
    /// their corridors. Ground is excluded: it is poured, not routed.
    #[must_use]
    pub fn auto_from_nets(
        net_names: &[String],
        clearance_mm: f64,
        signal_width_mm: f64,
        via_dia_mm: f64,
        via_drill_mm: f64,
    ) -> Self {
        let rails: Vec<String> = net_names
            .iter()
            .filter(|n| crate::model::is_supply_rail(n) && !crate::model::is_ground_net(n))
            .cloned()
            .collect();
        let mut rules = Self {
            net_classes: vec![NetClassRule {
                name: "Default".into(),
                nets: Vec::new(),
                clearance_mm,
                track_width_mm: signal_width_mm,
                via_dia_mm,
                via_drill_mm,
            }],
            constraints: Vec::new(),
        };
        if !rails.is_empty() && Self::RAIL_WIDTH_MM > signal_width_mm {
            rules.net_classes.push(NetClassRule {
                name: "Power".into(),
                nets: rails,
                clearance_mm,
                track_width_mm: Self::RAIL_WIDTH_MM,
                via_dia_mm,
                via_drill_mm,
            });
        }
        rules
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("not an s-expression: {0}")]
    Parse(String),
    #[error("custom-rule file must be a sequence of rule/version forms")]
    BadShape,
    #[error("net settings are not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl DesignRules {
    /// Parse a KiCad `.kicad_dru` custom-rule file. The file is a *sequence* of
    /// top-level forms, so it is wrapped in one list before parsing.
    ///
    /// @derives-from url:https://dev-docs.kicad.org/en/file-formats/sexpr-pcb/
    pub fn from_kicad_dru(text: &str) -> Result<Self, RulesError> {
        let root = Sexpr::parse(&format!("({text})")).map_err(RulesError::Parse)?;
        let items = root.as_list().ok_or(RulesError::BadShape)?;
        let mut rules = DesignRules::default();
        for item in items {
            if item.head() != Some("rule") {
                // `(version N)` and comments are ignored; anything else that is
                // not a rule is unexpected but harmless to skip.
                continue;
            }
            let name = item.nth_atom(1).unwrap_or("").to_string();
            let condition = item
                .get("condition")
                .and_then(|c| c.nth_atom(1))
                .map(str::to_string);
            let Some(constraint) = item.get("constraint") else {
                continue;
            };
            let kind_raw = constraint.nth_atom(1).unwrap_or("").to_string();
            let min_mm = constraint
                .get("min")
                .and_then(|m| m.nth_atom(1))
                .and_then(parse_mm);
            let max_mm = constraint
                .get("max")
                .and_then(|m| m.nth_atom(1))
                .and_then(parse_mm);
            rules.constraints.push(CustomConstraint {
                name,
                kind: ConstraintKind::from_token(&kind_raw),
                kind_raw,
                min_mm,
                max_mm,
                condition,
            });
        }
        // A `.kicad_dru` has no net classes; if it states unconditional
        // clearance/width/via constraints, adopt them as the Default class so
        // the router and checker honor them too — otherwise they'd route at
        // the fallback scalars and the emitted rules would disagree with the
        // copper (fail-quiet). Conditioned rules stay custom-only: only a rule
        // that applies to every item can set the global geometry.
        rules.adopt_unconditional_routing_class();
        Ok(rules)
    }

    /// Seed a `Default` net class from unconditional routing constraints when
    /// no classes were declared (the `.kicad_dru`-only case).
    fn adopt_unconditional_routing_class(&mut self) {
        if !self.net_classes.is_empty() {
            return;
        }
        let value = |kind: ConstraintKind, fallback: f64| -> f64 {
            self.constraints
                .iter()
                .find(|c| c.kind == kind && c.condition.is_none())
                .and_then(|c| c.min_mm)
                .unwrap_or(fallback)
        };
        let base = Self::jlcpcb_two_layer_default();
        let dflt = base.default_class().expect("built-in default");
        let clearance = value(ConstraintKind::Clearance, dflt.clearance_mm);
        let width = value(ConstraintKind::TrackWidth, dflt.track_width_mm);
        let via_dia = value(ConstraintKind::ViaDiameter, dflt.via_dia_mm);
        let via_drill = value(ConstraintKind::ViaDrill, dflt.via_drill_mm);
        self.net_classes.push(NetClassRule {
            name: "Default".into(),
            nets: Vec::new(),
            clearance_mm: clearance,
            track_width_mm: width,
            via_dia_mm: via_dia,
            via_drill_mm: via_drill,
        });
    }

    /// Parse the net classes out of a KiCad `.kicad_pro` project file.
    ///
    /// @derives-from url:https://dev-docs.kicad.org/en/file-formats/sexpr-pcb/
    ///   -- net_settings.classes encoding (project file is JSON)
    pub fn from_kicad_pro(text: &str) -> Result<Self, RulesError> {
        #[derive(Deserialize)]
        struct Project {
            #[serde(default)]
            net_settings: Option<NetSettings>,
        }
        #[derive(Deserialize)]
        struct NetSettings {
            #[serde(default)]
            classes: Vec<ProClass>,
        }
        #[derive(Deserialize)]
        struct ProClass {
            #[serde(default)]
            name: String,
            #[serde(default)]
            nets: Vec<String>,
            #[serde(default)]
            clearance: f64,
            #[serde(default)]
            track_width: f64,
            #[serde(default)]
            via_diameter: f64,
            #[serde(default)]
            via_drill: f64,
        }

        let project: Project = serde_json::from_str(text)?;
        let net_classes = project
            .net_settings
            .map(|s| {
                s.classes
                    .into_iter()
                    .map(|c| NetClassRule {
                        name: c.name,
                        nets: c.nets,
                        clearance_mm: c.clearance,
                        track_width_mm: c.track_width,
                        via_dia_mm: c.via_diameter,
                        via_drill_mm: c.via_drill,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(DesignRules {
            net_classes,
            constraints: Vec::new(),
        })
    }

    /// Serialize the constructive constraints to a KiCad `.kicad_dru`.
    /// Constraints are emitted in insertion order so the output is deterministic.
    pub fn to_kicad_dru(&self) -> String {
        let mut out = String::from(
            "(version 1)\n# Design rules generated by legion-of-bom; do not hand-edit.\n",
        );
        for c in &self.constraints {
            out.push_str(&format!("(rule \"{}\"\n", escape(&c.name)));
            let bounds = match (c.min_mm, c.max_mm) {
                (Some(min), Some(max)) => format!("(min {}mm) (max {}mm)", num(min), num(max)),
                (Some(min), None) => format!("(min {}mm)", num(min)),
                (None, Some(max)) => format!("(max {}mm)", num(max)),
                (None, None) => String::new(),
            };
            out.push_str(&format!(
                "\t(constraint {} {})\n",
                c.kind_raw,
                bounds.trim()
            ));
            if let Some(cond) = &c.condition {
                out.push_str(&format!("\t(condition \"{}\"))\n", escape(cond)));
            } else {
                out.push_str(")\n");
            }
        }
        out
    }

    /// The routing-relevant rules as a KiCad `.kicad_dru`, derived from the
    /// net classes so the emitted rule file, the router, and the first-party
    /// checker all read this one `DesignRules` object. For a single default
    /// class this is exactly the historical `routing_design_rules` output (an
    /// unconditional clearance and track-width pair); extra classes add
    /// per-class conditional rules so `kicad-cli pcb drc` judges a power net
    /// against its own class, not the signal default.
    ///
    /// @derives-from url:https://dev-docs.kicad.org/en/file-formats/sexpr-intro/ —
    ///   custom-rule `clearance`/`track_width` constraint encoding with net-class conditions
    #[must_use]
    pub fn routing_rules(&self) -> String {
        let mut out = String::from(
            "(version 1)\n# Routing rules generated by legion-of-bom from its DesignRules; \
             must match BoardOptions::route_options.\n",
        );
        // The default class governs any net that is not otherwise classified,
        // so its clearance/width are emitted unconditionally.
        if let Some(d) = self.default_class() {
            out.push_str(&format!(
                "(rule \"routed clearance\"   (constraint clearance (min {}mm)))\n",
                num(d.clearance_mm)
            ));
            out.push_str(&format!(
                "(rule \"routed track width\" (constraint track_width (min {}mm)))\n",
                num(d.track_width_mm)
            ));
        }
        // Every non-default class that tightens the rules gets its own
        // conditional pair, so a wider power track or looser clearance is
        // checked by KiCad against the class the router used.
        for c in &self.net_classes {
            if Some(c) == self.default_class() {
                continue;
            }
            let tighten_clearance = self
                .default_class()
                .is_none_or(|d| c.clearance_mm > d.clearance_mm);
            let tighten_width = self
                .default_class()
                .is_none_or(|d| c.track_width_mm > d.track_width_mm);
            if tighten_clearance {
                // The class-pair semantics (max of two classes' clearance) come
                // from KiCad's *native* net-class resolution, which only sees
                // classes the board itself declares. Custom rules below tighten
                // same-class pairs; cross-class pairs match once the board carries
                // net-class membership (see legion-of-bom-ntxl).
                out.push_str(&format!(
                    "(rule \"clearance class {}\" (constraint clearance (min {}mm)) \
                     (condition \"A.NetClass == '{}' && B.NetClass == '{}'\"))\n",
                    c.name,
                    num(c.clearance_mm),
                    escape(&c.name),
                    escape(&c.name)
                ));
            }
            if tighten_width {
                out.push_str(&format!(
                    "(rule \"track width class {}\" (constraint track_width (min {}mm)) \
                     (condition \"A.NetClass == '{}'\"))\n",
                    c.name,
                    num(c.track_width_mm),
                    escape(&c.name)
                ));
            }
        }
        out
    }
}

/// `"0.15mm"` or `"0.15"` -> `0.15`.
fn parse_mm(atom: &str) -> Option<f64> {
    atom.trim().trim_end_matches("mm").trim().parse().ok()
}

/// A number with no trailing zeros, matching KiCad's own `mm` formatting.
fn num(v: f64) -> String {
    let s = format!("{v}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Escape a name/condition for a quoted KiCad string.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legion_of_bom_routing_rules() {
        // Exactly what `fab::routing_design_rules` emits.
        let rules = DesignRules::from_kicad_dru(
            "(version 1)\n\
             (rule \"routed clearance\"   (constraint clearance (min 0.15mm)))\n\
             (rule \"routed track width\" (constraint track_width (min 0.2mm)))\n",
        )
        .unwrap();
        assert_eq!(rules.constraints.len(), 2);
        assert_eq!(rules.constraints[0].kind, ConstraintKind::Clearance);
        assert_eq!(rules.constraints[0].min_mm, Some(0.15));
        assert_eq!(rules.constraints[1].kind, ConstraintKind::TrackWidth);
        assert_eq!(rules.constraints[1].min_mm, Some(0.2));
    }

    #[test]
    fn preserves_a_condition_and_an_unknown_constraint() {
        let rules = DesignRules::from_kicad_dru(
            "(version 1)\n\
             (rule \"power is wide\"\n\
             \t(constraint track_width (min 0.5mm))\n\
             \t(condition \"A.NetClass == 'Power'\"))\n\
             (rule \"weird\" (constraint frobnicate (min 1mm)))\n",
        )
        .unwrap();
        assert_eq!(
            rules.constraints[0].condition.as_deref(),
            Some("A.NetClass == 'Power'")
        );
        // Unknown kinds survive for coverage reporting rather than vanishing.
        assert_eq!(rules.constraints[1].kind, ConstraintKind::Other);
        assert_eq!(rules.constraints[1].kind_raw, "frobnicate");
    }

    #[test]
    fn parses_kicad_pro_net_classes() {
        let pro = r#"{
            "net_settings": {
                "classes": [
                    {"name":"Default","clearance":0.2,"track_width":0.25,
                     "via_diameter":0.8,"via_drill":0.4,"nets":["SIG"]},
                    {"name":"Power","clearance":0.25,"track_width":0.5,
                     "via_diameter":1.0,"via_drill":0.5,"nets":["+3V3","GND"]}
                ]
            }
        }"#;
        let rules = DesignRules::from_kicad_pro(pro).unwrap();
        assert_eq!(rules.net_classes.len(), 2);
        assert_eq!(rules.net_classes[1].name, "Power");
        assert_eq!(rules.net_classes[1].track_width_mm, 0.5);
        assert_eq!(rules.net_classes[1].nets, vec!["+3V3", "GND"]);
    }

    #[test]
    fn export_round_trips_through_the_parser() {
        let original = DesignRules {
            net_classes: Vec::new(),
            constraints: vec![
                CustomConstraint {
                    name: "routed clearance".into(),
                    kind: ConstraintKind::Clearance,
                    kind_raw: "clearance".into(),
                    min_mm: Some(0.15),
                    max_mm: None,
                    condition: None,
                },
                CustomConstraint {
                    name: "bounded".into(),
                    kind: ConstraintKind::HoleSize,
                    kind_raw: "hole_size".into(),
                    min_mm: Some(0.2),
                    max_mm: Some(6.3),
                    condition: Some("A.Type == 'Via'".into()),
                },
            ],
        };
        let text = original.to_kicad_dru();
        let parsed = DesignRules::from_kicad_dru(&text).unwrap();
        assert_eq!(parsed.constraints, original.constraints);
    }

    #[test]
    fn malformed_dru_is_an_error_not_a_silent_pass() {
        assert!(DesignRules::from_kicad_dru("(rule \"oops\"").is_err());
    }

    #[test]
    fn pair_clearance_uses_the_stricter_class() {
        let rules = DesignRules {
            net_classes: vec![
                NetClassRule {
                    name: "Default".into(),
                    nets: vec![],
                    clearance_mm: 0.2,
                    track_width_mm: 0.25,
                    via_dia_mm: 0.8,
                    via_drill_mm: 0.4,
                },
                NetClassRule {
                    name: "Power".into(),
                    nets: vec!["+3V3".into(), "GND".into()],
                    clearance_mm: 0.3,
                    track_width_mm: 0.5,
                    via_dia_mm: 1.0,
                    via_drill_mm: 0.5,
                },
            ],
            constraints: vec![],
        };
        // KiCad semantics: pair clearance is the max of the two classes.
        assert_eq!(rules.pair_clearance("+3V3", "SIG"), 0.3);
        assert_eq!(rules.pair_clearance("SIG", "+3V3"), 0.3);
        assert_eq!(rules.pair_clearance("+3V3", "GND"), 0.3);
        assert_eq!(rules.pair_clearance("SIG", "SIG2"), 0.2);
        assert_eq!(rules.net_class("SIG").unwrap().name, "Default");
        assert_eq!(rules.net_class("+3V3").unwrap().name, "Power");
        assert_eq!(rules.strictest_clearance(), Some(0.3));
        assert_eq!(rules.widest_track(), Some(0.5));
    }

    #[test]
    fn unconditional_dru_constraints_become_the_default_class() {
        // A `.kicad_dru` with no net classes must still drive the router and
        // checker, not just the emitted file — otherwise two answers about one
        // board differ depending on which code path read the rules.
        let rules = DesignRules::from_kicad_dru(
            "(version 1)\n\
             (rule \"dense clearance\" (constraint clearance (min 0.13mm)))\n\
             (rule \"wide power\" (constraint track_width (min 0.4mm)) \
              (condition \"A.NetClass == 'Power'\"))\n",
        )
        .unwrap();
        let dflt = rules.default_class().expect("class adopted");
        assert_eq!(dflt.clearance_mm, 0.13);
        // A *conditioned* width rule does not set the global width.
        assert_eq!(dflt.track_width_mm, 0.25);
    }

    #[test]
    fn routing_rules_emit_default_plus_tightened_classes() {
        let rules = DesignRules {
            net_classes: vec![
                NetClassRule {
                    name: "Default".into(),
                    nets: vec![],
                    clearance_mm: 0.2,
                    track_width_mm: 0.25,
                    via_dia_mm: 0.8,
                    via_drill_mm: 0.4,
                },
                NetClassRule {
                    name: "Power".into(),
                    nets: vec!["+3V3".into()],
                    clearance_mm: 0.3,
                    track_width_mm: 0.5,
                    via_dia_mm: 1.0,
                    via_drill_mm: 0.5,
                },
                NetClassRule {
                    name: "Antenna".into(),
                    nets: vec!["ANT".into()],
                    clearance_mm: 0.15,
                    track_width_mm: 0.2,
                    via_dia_mm: 0.6,
                    via_drill_mm: 0.3,
                },
            ],
            constraints: vec![],
        };
        let dru = rules.routing_rules();
        assert!(dru.starts_with("(version 1)"), "{dru}");
        // Exactly one `(version 1)` so the file is loadable by kicad-cli.
        assert_eq!(dru.matches("(version 1)").count(), 1);
        assert!(dru.contains("(rule \"routed clearance\"   (constraint clearance (min 0.2mm)))"));
        assert!(dru.contains("(rule \"routed track width\" (constraint track_width (min 0.25mm)))"));
        // The stricter Power class gets conditional rules…
        assert!(dru.contains("A.NetClass == 'Power' && B.NetClass == 'Power'"));
        assert!(dru.contains("track_width (min 0.5mm)"));
        // …and a looser class is not restated (it can only be under-cut by
        // the router's strictest grid, which the base rule already covers).
        assert!(!dru.contains("Antenna"), "{dru}");
    }

    #[test]
    fn auto_from_nets_widens_only_the_rails() {
        let names: Vec<String> = ["+12V", "-12V", "SIG_OUT", "GND", "3V3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rules = DesignRules::auto_from_nets(&names, 0.2, 0.25, 0.8, 0.4);
        assert_eq!(rules.net_classes.len(), 2, "Default + Power");
        let width = |n: &str| rules.net_class(n).unwrap().track_width_mm;
        assert_eq!(width("SIG_OUT"), 0.25);
        assert_eq!(width("GND"), 0.25, "ground pours, never a rail class");
        assert_eq!(width("+12V"), 0.4);
        assert_eq!(width("3V3"), 0.4);
        // Clearance is deliberately NOT tightened: every class carries the
        // board default so the shared grid costs signals nothing.
        assert_eq!(rules.strictest_clearance(), Some(0.2));
        assert_eq!(rules.pair_clearance("+12V", "SIG_OUT"), 0.2);
    }

    #[test]
    fn auto_from_nets_is_a_no_shape_change_without_rails() {
        let names: Vec<String> = ["SIG", "GND", "CV_IN"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rules = DesignRules::auto_from_nets(&names, 0.2, 0.25, 0.8, 0.4);
        // No rails (CV_IN is a control-voltage *signal*, not a supply) → only
        // the Default class, so behaviour is identical to rules being absent.
        assert_eq!(rules.net_classes.len(), 1);
        assert_eq!(rules.pair_clearance("SIG", "CV_IN"), 0.2);
        assert_eq!(rules.widest_track(), Some(0.25));
    }

    #[test]
    fn default_rules_match_the_router_fallback_scalars() {
        // The single source and the router's fallback must agree exactly: a
        // `design_rules: None` board and a board with the default rules are
        // the same board, and drift here would reappear as two DRC answers.
        let rules = DesignRules::jlcpcb_two_layer_default();
        let dflt = rules.default_class().expect("default class");
        let opts = crate::route::RouteOptions::default();
        assert_eq!(opts.clearance_mm, dflt.clearance_mm);
        assert_eq!(opts.signal_width_mm, dflt.track_width_mm);
        assert_eq!(opts.via_size_mm, dflt.via_dia_mm);
        assert_eq!(opts.via_drill_mm, dflt.via_drill_mm);
        assert_eq!(opts.edge_clearance_mm, rules.edge_clearance_mm().unwrap());
        assert_eq!(opts.grid_clearance(), dflt.clearance_mm);
    }
}
