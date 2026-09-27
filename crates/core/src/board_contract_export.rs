//! Producer-side export of the lob board electromechanical contract.
//!
//! Geometry comes only from the exact [`BoardArtifacts`] generation pass and
//! explicit system metadata. No STEP/STL/mesh input exists on this boundary.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::{
    board::{BoardArtifacts, Placement},
    board_contract::{
        BoardDatum, BoardElectromechanicalArtifact, BoardEnvelope, BoardMassProperties,
        BoardRevision, CatalogIdentity, ConnectorInterface, ContractProvenance, EnvelopeKind,
        MechanicalMountingHole, Point2Mm, Point3Mm, ThermalPowerMetadata, UnitVector3,
        BOARD_ELECTROMECHANICAL_SCHEMA,
    },
    source::CircuitSource,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardContractExportSpec {
    pub board_id: String,
    pub board_revision: BoardRevision,
    pub mounting_holes: Vec<MountingHoleExportSource>,
    pub connectors: Vec<ConnectorExportSource>,
    /// Antenna, cable, and service envelopes declared by the system design.
    /// Component height envelopes are derived from generated footprint facts.
    #[serde(default)]
    pub system_envelopes: Vec<BoardEnvelope>,
    pub mass_properties: Option<BoardMassProperties>,
    pub thermal_power: Option<ThermalPowerMetadata>,
    pub provenance: Option<ContractProvenance>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountingHoleExportSource {
    pub reference_designator: String,
    pub finished_diameter_mm: f64,
    pub diameter_tolerance_mm: f64,
    pub position_tolerance_mm: f64,
    pub plated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorExportSource {
    pub reference_designator: String,
    pub interface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_identity: Option<CatalogIdentity>,
    /// Mating direction in footprint-local coordinates. Export rotates and, on
    /// the back side, mirrors it with the generated placement.
    pub local_mating_direction: UnitVector3,
}

#[derive(Debug, thiserror::Error)]
pub enum BoardContractExportError {
    #[error("generated board has no outline")]
    MissingOutline,
    #[error("required electromechanical fact {0} is missing")]
    MissingFact(&'static str),
    #[error("source component {0:?} does not exist")]
    UnknownComponent(String),
    #[error("source component {0:?} was not placed in this board generation pass")]
    MissingPlacement(String),
    #[error("source component {0:?} has no generated footprint facts")]
    MissingFootprintFacts(String),
    #[error("component {0:?} is declared more than once")]
    DuplicateComponent(String),
    #[error("invalid board contract: {0}")]
    InvalidContract(#[from] crate::board_contract::BoardContractError),
}

pub fn export_board_electromechanical_contract(
    circuit: &dyn CircuitSource,
    board: &BoardArtifacts,
    spec: &BoardContractExportSpec,
) -> Result<BoardElectromechanicalArtifact, BoardContractExportError> {
    let (min_x, min_y, max_x, max_y) = board
        .outline_mm
        .ok_or(BoardContractExportError::MissingOutline)?;
    let mass_properties = spec
        .mass_properties
        .clone()
        .ok_or(BoardContractExportError::MissingFact("mass_properties"))?;
    let thermal_power = spec
        .thermal_power
        .clone()
        .ok_or(BoardContractExportError::MissingFact("thermal_power"))?;
    let provenance = spec
        .provenance
        .clone()
        .ok_or(BoardContractExportError::MissingFact("provenance"))?;
    let parts = circuit
        .parts()
        .iter()
        .map(|part| (part.refdes.0.as_str(), part))
        .collect::<HashMap<_, _>>();

    let mut seen = BTreeSet::new();
    let mounting_holes = spec
        .mounting_holes
        .iter()
        .map(|source| {
            component(&parts, board, &source.reference_designator)?;
            if !seen.insert(source.reference_designator.as_str()) {
                return Err(BoardContractExportError::DuplicateComponent(
                    source.reference_designator.clone(),
                ));
            }
            let placement = board.placements[&source.reference_designator];
            Ok(MechanicalMountingHole {
                id: source.reference_designator.clone(),
                datum_id: "pcb-origin".into(),
                center_mm: local_point(placement, min_x, min_y),
                finished_diameter_mm: source.finished_diameter_mm,
                diameter_tolerance_mm: source.diameter_tolerance_mm,
                position_tolerance_mm: source.position_tolerance_mm,
                plated: source.plated,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    seen.clear();
    let connectors = spec
        .connectors
        .iter()
        .map(|source| {
            component(&parts, board, &source.reference_designator)?;
            if !seen.insert(source.reference_designator.as_str()) {
                return Err(BoardContractExportError::DuplicateComponent(
                    source.reference_designator.clone(),
                ));
            }
            let placement = board.placements[&source.reference_designator];
            let point = local_point(placement, min_x, min_y);
            Ok(ConnectorInterface {
                id: source.reference_designator.clone(),
                reference_designator: source.reference_designator.clone(),
                interface: source.interface.clone(),
                catalog_identity: source.catalog_identity.clone(),
                datum_id: "pcb-origin".into(),
                location_mm: Point3Mm {
                    x: point.x,
                    y: point.y,
                    z: if placement.back {
                        0.0
                    } else {
                        board.thickness_mm
                    },
                },
                mating_direction: placed_direction(source.local_mating_direction, placement),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut envelopes = Vec::new();
    for (reference, placement) in &board.placements {
        let facts = board
            .facts
            .get(reference)
            .ok_or_else(|| BoardContractExportError::MissingFootprintFacts(reference.clone()))?;
        let (x0, y0, x1, y1) = facts.keepout_at_rot(
            placement.x_mm,
            placement.y_mm,
            placement.back,
            placement.rotation_deg,
        );
        let (z_min_mm, z_max_mm) = if placement.back {
            (-facts.height_mm, 0.0)
        } else {
            (board.thickness_mm, board.thickness_mm + facts.height_mm)
        };
        envelopes.push(BoardEnvelope {
            id: format!("{reference}-height"),
            kind: EnvelopeKind::Height,
            datum_id: "pcb-origin".into(),
            footprint_mm: rectangle(x0 - min_x, y0 - min_y, x1 - min_x, y1 - min_y),
            z_min_mm,
            z_max_mm,
            connector_id: None,
        });
    }
    envelopes.extend(spec.system_envelopes.iter().cloned());

    let artifact = BoardElectromechanicalArtifact {
        schema: BOARD_ELECTROMECHANICAL_SCHEMA.into(),
        board_id: spec.board_id.clone(),
        board_revision: spec.board_revision.clone(),
        outline_mm: rectangle(0.0, 0.0, max_x - min_x, max_y - min_y),
        thickness_mm: board.thickness_mm,
        datums: vec![BoardDatum {
            id: "pcb-origin".into(),
            origin_mm: Point3Mm {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            x_axis: UnitVector3 {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
            y_axis: UnitVector3 {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            },
        }],
        mounting_holes,
        connectors,
        envelopes,
        mass_properties,
        thermal_power,
        provenance,
    };
    artifact.validate()?;
    Ok(artifact)
}

fn component<'a>(
    parts: &HashMap<&str, &'a crate::model::Part>,
    board: &BoardArtifacts,
    reference: &str,
) -> Result<&'a crate::model::Part, BoardContractExportError> {
    let part = parts
        .get(reference)
        .copied()
        .ok_or_else(|| BoardContractExportError::UnknownComponent(reference.into()))?;
    if !board.placements.contains_key(reference) {
        return Err(BoardContractExportError::MissingPlacement(reference.into()));
    }
    if !board.facts.contains_key(reference) {
        return Err(BoardContractExportError::MissingFootprintFacts(
            reference.into(),
        ));
    }
    Ok(part)
}

fn local_point(placement: Placement, min_x: f64, min_y: f64) -> Point2Mm {
    Point2Mm {
        x: placement.x_mm - min_x,
        y: placement.y_mm - min_y,
    }
}

fn placed_direction(vector: UnitVector3, placement: Placement) -> UnitVector3 {
    let local_y = if placement.back { -vector.y } else { vector.y };
    let (sin, cos) = placement.rotation_deg.to_radians().sin_cos();
    UnitVector3 {
        x: vector.x * cos + local_y * sin,
        y: local_y * cos - vector.x * sin,
        z: if placement.back { -vector.z } else { vector.z },
    }
}

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<Point2Mm> {
    vec![
        Point2Mm { x: x0, y: y0 },
        Point2Mm { x: x1, y: y0 },
        Point2Mm { x: x1, y: y1 },
        Point2Mm { x: x0, y: y1 },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        board::{generate_board_artifacts, BoardOptions, PartFacts, Placer},
        model::{Circuit, Part},
    };
    use std::{collections::HashMap, path::PathBuf};

    struct FlightComputerPlacement;

    impl Placer for FlightComputerPlacement {
        fn place(
            &self,
            _circuit: &dyn CircuitSource,
            _facts: &HashMap<String, PartFacts>,
        ) -> HashMap<String, Placement> {
            [
                ("H1", 4.0, 4.0),
                ("H2", 76.0, 26.0),
                ("J1", 78.0, 15.0),
                ("U1", 40.0, 15.0),
            ]
            .into_iter()
            .map(|(reference, x_mm, y_mm)| {
                (
                    reference.into(),
                    Placement {
                        x_mm,
                        y_mm,
                        rotation_deg: 0.0,
                        back: false,
                    },
                )
            })
            .collect()
        }
    }

    fn footprint_dir() -> PathBuf {
        // Unique per call, not merely per process: two tests that both build a
        // flight computer run in parallel and would otherwise delete each
        // other's footprint library mid-run.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "lob-flight-computer-contract-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let library = root.join("Flight.pretty");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(
            library.join("Hole.kicad_mod"),
            r#"(footprint "Hole" (layer "F.Cu")
                (pad "" np_thru_hole circle (at 0 0) (size 3.2 3.2) (drill 3.2) (layers "*.Cu" "*.Mask")))"#,
        )
        .unwrap();
        std::fs::write(
            library.join("UsbC.kicad_mod"),
            r#"(footprint "UsbC" (layer "F.Cu")
                (fp_rect (start -4 -3) (end 4 3) (stroke (width 0.1) (type default)) (fill none) (layer "F.CrtYd"))
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#,
        )
        .unwrap();
        std::fs::write(
            library.join("Controller.kicad_mod"),
            r#"(footprint "Controller" (layer "F.Cu")
                (fp_rect (start -5 -5) (end 5 5) (stroke (width 0.1) (type default)) (fill none) (layer "F.CrtYd"))
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#,
        )
        .unwrap();
        root
    }

    fn generated_flight_computer() -> (Circuit, BoardArtifacts, f64, PathBuf) {
        let footprints = footprint_dir();
        let circuit = Circuit {
            name: "flight-computer".into(),
            parts: vec![
                Part::new("H1", "M3").with_footprint("Flight:Hole"),
                Part::new("H2", "M3").with_footprint("Flight:Hole"),
                Part::new("J1", "USB-C").with_footprint("Flight:UsbC"),
                Part::new("U1", "flight controller").with_footprint("Flight:Controller"),
            ],
            nets: Vec::new(),
        };
        let mut options = BoardOptions::new(&footprints);
        options.placer = Box::new(FlightComputerPlacement);
        options.router = None;
        options.fixed_outline = Some((0.0, 0.0, 80.0, 30.0));
        options.thickness_mm = 1.8;
        let artifacts = generate_board_artifacts(&circuit, &options).unwrap();
        (circuit, artifacts, options.thickness_mm, footprints)
    }

    fn spec() -> BoardContractExportSpec {
        BoardContractExportSpec {
            board_id: "flight-computer".into(),
            board_revision: BoardRevision {
                design: "A".into(),
                variant: "flight".into(),
            },
            mounting_holes: ["H1", "H2"]
                .into_iter()
                .map(|reference| MountingHoleExportSource {
                    reference_designator: reference.into(),
                    finished_diameter_mm: 3.2,
                    diameter_tolerance_mm: 0.1,
                    position_tolerance_mm: 0.2,
                    plated: false,
                })
                .collect(),
            connectors: vec![ConnectorExportSource {
                reference_designator: "J1".into(),
                interface: "usb-c-receptacle".into(),
                catalog_identity: Some(CatalogIdentity {
                    manufacturer: "GCT".into(),
                    part_number: "USB4105-GF-A".into(),
                }),
                local_mating_direction: UnitVector3 {
                    x: 1.0,
                    y: 0.0,
                    z: 0.0,
                },
            }],
            system_envelopes: vec![BoardEnvelope {
                id: "usb-service".into(),
                kind: EnvelopeKind::Service,
                datum_id: "pcb-origin".into(),
                footprint_mm: rectangle(72.0, 9.0, 80.0, 21.0),
                z_min_mm: 0.0,
                z_max_mm: 25.0,
                connector_id: Some("J1".into()),
            }],
            mass_properties: Some(BoardMassProperties {
                mass_g: 42.0,
                center_of_gravity_mm: Point3Mm {
                    x: 39.0,
                    y: 15.0,
                    z: 2.0,
                },
            }),
            thermal_power: Some(ThermalPowerMetadata {
                operating_temperature_min_c: -20.0,
                operating_temperature_max_c: 70.0,
                maximum_dissipation_w: 8.0,
                power_inputs: vec![crate::board_contract::PowerInput {
                    id: "usb-vbus".into(),
                    connector_id: "J1".into(),
                    nominal_voltage_v: 5.0,
                    maximum_current_a: 3.0,
                }],
            }),
            provenance: Some(ContractProvenance {
                producer: "legion-of-bom".into(),
                source_artifact: "flight-computer.kicad_pcb".into(),
                source_revision: "git:fixture".into(),
                generated_by: "lob fixture".into(),
                generated_at: "2026-09-25T00:00:00Z".into(),
                digest: "sha256:fixture".into(),
            }),
        }
    }

    #[test]
    fn flight_computer_contract_uses_generated_board_facts() {
        let (circuit, board, thickness, footprints) = generated_flight_computer();
        assert_eq!(thickness, board.thickness_mm);
        let artifact = export_board_electromechanical_contract(&circuit, &board, &spec()).unwrap();
        assert_eq!(artifact.outline_mm, rectangle(0.0, 0.0, 80.0, 30.0));
        assert_eq!(artifact.thickness_mm, 1.8);
        assert_eq!(
            artifact.mounting_holes[0].center_mm,
            Point2Mm {
                x: board.placements["H1"].x_mm,
                y: board.placements["H1"].y_mm,
            }
        );
        assert_eq!(
            artifact.connectors[0].location_mm.x,
            board.placements["J1"].x_mm
        );
        assert_eq!(
            artifact
                .envelopes
                .iter()
                .filter(|envelope| envelope.kind == EnvelopeKind::Height)
                .count(),
            board.facts.len()
        );
        assert!(board.pcb.contains("(thickness 1.8)"));
        std::fs::remove_dir_all(footprints).unwrap();
    }

    #[test]
    fn missing_required_system_fact_fails_loudly() {
        let (circuit, board, _thickness, footprints) = generated_flight_computer();
        let mut incomplete = spec();
        incomplete.mass_properties = None;
        assert!(matches!(
            export_board_electromechanical_contract(&circuit, &board, &incomplete),
            Err(BoardContractExportError::MissingFact("mass_properties"))
        ));
        std::fs::remove_dir_all(footprints).unwrap();
    }
}
