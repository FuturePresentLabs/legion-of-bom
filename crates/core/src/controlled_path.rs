//! Conductive-path graph derived from real net attachments and sourced part behavior.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::{model::Part, source::CircuitSource};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConductiveEdge {
    pub part: String,
    pub path: String,
    pub from_net: String,
    pub to_net: String,
    pub control_net: Option<String>,
    pub control_identity: Option<String>,
    pub default_conducting: bool,
    pub reverse_conducting: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphFinding {
    pub part: String,
    pub path: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ControlSource {
    pub part: String,
    pub kind: String,
    pub net: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ConductiveGraph {
    pub edges: Vec<ConductiveEdge>,
    pub control_sources: Vec<ControlSource>,
    pub unresolved: Vec<GraphFinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PathEvidence {
    pub source_net: String,
    pub target_net: String,
    pub reachable: bool,
    pub reachable_by_default: bool,
    pub minimum_independent_controls: Option<usize>,
    pub minimum_control_nets: Vec<String>,
    pub minimum_control_source_kinds: Vec<String>,
    pub reverse_reachable: bool,
}

fn field<'a>(part: &'a Part, key: &str) -> Option<&'a str> {
    part.fields.get(key).map(String::as_str)
}

fn boolean(part: &Part, key: &str) -> Option<bool> {
    match field(part, key)?.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn net_for_pin(circuit: &dyn CircuitSource, part: &Part, wanted: &str) -> Option<String> {
    circuit.nets().iter().find_map(|net| {
        net.pins
            .iter()
            .any(|pin| {
                pin.refdes == part.refdes
                    && (pin.pin.eq_ignore_ascii_case(wanted)
                        || pin
                            .function
                            .as_deref()
                            .is_some_and(|name| name.eq_ignore_ascii_case(wanted)))
            })
            .then(|| net.name.clone())
    })
}

/// Build a graph only from verified `Conduction.*` fields carried by the
/// rendered parts and the pins actually attached to nets.
#[must_use]
pub fn derive(circuit: &dyn CircuitSource) -> ConductiveGraph {
    let mut graph = ConductiveGraph::default();
    for part in circuit.parts() {
        let output_indices = part
            .fields
            .keys()
            .filter_map(|key| {
                key.strip_prefix("ControlOutput.")?
                    .split('.')
                    .next()?
                    .parse::<usize>()
                    .ok()
            })
            .collect::<BTreeSet<_>>();
        for index in output_indices {
            let prefix = format!("ControlOutput.{index}");
            let kind = field(part, &format!("{prefix}.Kind"));
            let pin = field(part, &format!("{prefix}.Pin"));
            match (kind, pin.and_then(|pin| net_for_pin(circuit, part, pin))) {
                (Some(kind), Some(net)) => graph.control_sources.push(ControlSource {
                    part: part.refdes.0.clone(),
                    kind: kind.into(),
                    net,
                }),
                _ => graph.unresolved.push(GraphFinding {
                    part: part.refdes.0.clone(),
                    path: format!("control-output-{index}"),
                    detail: "control output kind/pin is missing or its pin is not attached".into(),
                }),
            }
        }
        let mut indices = part
            .fields
            .keys()
            .filter_map(|key| {
                key.strip_prefix("Conduction.")?
                    .split('.')
                    .next()?
                    .parse::<usize>()
                    .ok()
            })
            .collect::<BTreeSet<_>>();
        for index in std::mem::take(&mut indices) {
            let prefix = format!("Conduction.{index}");
            let id = field(part, &format!("{prefix}.Id"))
                .unwrap_or("unnamed")
                .to_string();
            let from_pin = field(part, &format!("{prefix}.FromPin"));
            let to_pin = field(part, &format!("{prefix}.ToPin"));
            let control_pin = field(part, &format!("{prefix}.ControlPin"));
            let Some(from_net) = from_pin.and_then(|pin| net_for_pin(circuit, part, pin)) else {
                graph.unresolved.push(GraphFinding {
                    part: part.refdes.0.clone(),
                    path: id,
                    detail: "from pin is missing or not attached to a net".into(),
                });
                continue;
            };
            let Some(to_net) = to_pin.and_then(|pin| net_for_pin(circuit, part, pin)) else {
                graph.unresolved.push(GraphFinding {
                    part: part.refdes.0.clone(),
                    path: id,
                    detail: "to pin is missing or not attached to a net".into(),
                });
                continue;
            };
            let control_net = control_pin.and_then(|pin| net_for_pin(circuit, part, pin));
            if control_pin.is_some() && control_net.is_none() {
                graph.unresolved.push(GraphFinding {
                    part: part.refdes.0.clone(),
                    path: id.clone(),
                    detail: "control pin is not attached to a net".into(),
                });
            }
            let default_conducting = boolean(part, &format!("{prefix}.DefaultConducting"));
            let reverse_conducting = boolean(part, &format!("{prefix}.ReverseConducting"));
            let (Some(default_conducting), Some(reverse_conducting)) =
                (default_conducting, reverse_conducting)
            else {
                graph.unresolved.push(GraphFinding {
                    part: part.refdes.0.clone(),
                    path: id,
                    detail: "default/reverse conduction behavior is missing or malformed".into(),
                });
                continue;
            };
            graph.edges.push(ConductiveEdge {
                part: part.refdes.0.clone(),
                path: id,
                from_net,
                to_net,
                control_net,
                control_identity: field(part, &format!("{prefix}.ControlIdentity"))
                    .map(str::to_string),
                default_conducting,
                reverse_conducting,
            });
        }
    }
    graph.edges.sort_by(|a, b| {
        (&a.from_net, &a.to_net, &a.part, &a.path).cmp(&(&b.from_net, &b.to_net, &b.part, &b.path))
    });
    graph
        .control_sources
        .sort_by(|a, b| (&a.net, &a.kind, &a.part).cmp(&(&b.net, &b.kind, &b.part)));
    graph
}

fn paths(
    graph: &ConductiveGraph,
    source: &str,
    target: &str,
    default_only: bool,
) -> Vec<BTreeSet<String>> {
    fn walk(
        graph: &ConductiveGraph,
        current: &str,
        target: &str,
        default_only: bool,
        visited: &mut BTreeSet<String>,
        controls: &BTreeSet<String>,
        out: &mut Vec<BTreeSet<String>>,
    ) {
        if current == target {
            out.push(controls.clone());
            return;
        }
        if !visited.insert(current.to_string()) {
            return;
        }
        for edge in graph
            .edges
            .iter()
            .filter(|edge| edge.from_net == current && (!default_only || edge.default_conducting))
        {
            let mut next_controls = controls.clone();
            if let Some(control) = &edge.control_net {
                next_controls.insert(control.clone());
            }
            walk(
                graph,
                &edge.to_net,
                target,
                default_only,
                visited,
                &next_controls,
                out,
            );
        }
        visited.remove(current);
    }
    let mut out = Vec::new();
    walk(
        graph,
        source,
        target,
        default_only,
        &mut BTreeSet::new(),
        &BTreeSet::new(),
        &mut out,
    );
    out
}

#[must_use]
pub fn analyze(graph: &ConductiveGraph, source_net: &str, target_net: &str) -> PathEvidence {
    let all = paths(graph, source_net, target_net, false);
    let default = paths(graph, source_net, target_net, true);
    let minimum = all.iter().min_by_key(|controls| controls.len());
    let minimum_control_nets = minimum
        .map(|controls| controls.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let minimum_control_source_kinds = graph
        .control_sources
        .iter()
        .filter(|source| minimum_control_nets.contains(&source.net))
        .map(|source| source.kind.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut reverse = graph.clone();
    reverse.edges = graph
        .edges
        .iter()
        .filter(|edge| edge.reverse_conducting)
        .map(|edge| {
            let mut edge = edge.clone();
            std::mem::swap(&mut edge.from_net, &mut edge.to_net);
            edge
        })
        .collect();
    PathEvidence {
        source_net: source_net.into(),
        target_net: target_net.into(),
        reachable: !all.is_empty(),
        reachable_by_default: !default.is_empty(),
        minimum_independent_controls: minimum.map(BTreeSet::len),
        minimum_control_nets,
        minimum_control_source_kinds,
        reverse_reachable: !paths(&reverse, target_net, source_net, false).is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Circuit, Net, Part, PinRef};

    fn controlled_part(refdes: &str, control: &str) -> Part {
        let mut part = Part::new(refdes, "controlled switch");
        for (suffix, value) in [
            ("Id", "channel"),
            ("FromPin", "IN"),
            ("ToPin", "OUT"),
            ("ControlPin", "EN"),
            ("ControlIdentity", "enable"),
            ("DefaultConducting", "false"),
            ("ReverseConducting", "false"),
        ] {
            part.fields
                .insert(format!("Conduction.0.{suffix}"), value.into());
        }
        part.fields
            .insert("test.control_net".into(), control.into());
        part
    }

    fn control_source(refdes: &str, kind: &str) -> Part {
        let mut part = Part::new(refdes, "supervisor");
        part.fields
            .insert("ControlOutput.0.Kind".into(), kind.into());
        part.fields
            .insert("ControlOutput.0.Pin".into(), "OUT".into());
        part
    }

    #[test]
    fn derives_two_independent_series_controls_from_actual_nets() {
        let mut circuit = Circuit::new("controlled");
        circuit.parts.extend([
            controlled_part("Q1", "ARM_A"),
            controlled_part("Q2", "ARM_B"),
            control_source("U1", "reset"),
            control_source("U2", "watchdog"),
        ]);
        circuit.nets = vec![
            Net::new("SOURCE", vec![PinRef::new("Q1", "1").with_function("IN")]),
            Net::new(
                "MID",
                vec![
                    PinRef::new("Q1", "2").with_function("OUT"),
                    PinRef::new("Q2", "1").with_function("IN"),
                ],
            ),
            Net::new("TARGET", vec![PinRef::new("Q2", "2").with_function("OUT")]),
            Net::new(
                "ARM_A",
                vec![
                    PinRef::new("Q1", "3").with_function("EN"),
                    PinRef::new("U1", "1").with_function("OUT"),
                ],
            ),
            Net::new(
                "ARM_B",
                vec![
                    PinRef::new("Q2", "3").with_function("EN"),
                    PinRef::new("U2", "1").with_function("OUT"),
                ],
            ),
        ];
        let graph = derive(&circuit);
        let evidence = analyze(&graph, "SOURCE", "TARGET");
        assert!(evidence.reachable);
        assert!(!evidence.reachable_by_default);
        assert_eq!(evidence.minimum_independent_controls, Some(2));
        assert_eq!(evidence.minimum_control_nets, ["ARM_A", "ARM_B"]);
        assert_eq!(evidence.minimum_control_source_kinds, ["reset", "watchdog"]);
        assert!(!evidence.reverse_reachable);
    }

    #[test]
    fn same_control_net_is_one_common_cause_not_two_inhibits() {
        let mut circuit = Circuit::new("common-cause");
        circuit
            .parts
            .extend([controlled_part("Q1", "ARM"), controlled_part("Q2", "ARM")]);
        circuit.nets = vec![
            Net::new("SOURCE", vec![PinRef::new("Q1", "1").with_function("IN")]),
            Net::new(
                "MID",
                vec![
                    PinRef::new("Q1", "2").with_function("OUT"),
                    PinRef::new("Q2", "1").with_function("IN"),
                ],
            ),
            Net::new("TARGET", vec![PinRef::new("Q2", "2").with_function("OUT")]),
            Net::new(
                "ARM",
                vec![
                    PinRef::new("Q1", "3").with_function("EN"),
                    PinRef::new("Q2", "3").with_function("EN"),
                ],
            ),
        ];
        assert_eq!(
            analyze(&derive(&circuit), "SOURCE", "TARGET").minimum_independent_controls,
            Some(1)
        );
    }
}
