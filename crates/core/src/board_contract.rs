//! Versioned electromechanical facts exported by legion-of-bom.
//!
//! legion-of-bom owns this artifact because its PCB design, component
//! placement, and source revision are the semantic source of truth. Mechanical
//! consumers may position the immutable board and design mating hardware from
//! these facts; they must not rewrite PCB geometry, infer connector identity,
//! or publish a modified artifact under the original provenance. A PCB change
//! requires a newly generated artifact and `board_revision`.
//!
//! Compatibility is deliberately exact for v1: consumers accept
//! [`BOARD_ELECTROMECHANICAL_SCHEMA`] and fail closed on unknown fields or
//! schema versions. Additive or semantic changes therefore require a new
//! version plus an explicit adapter rather than silent interpretation.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const BOARD_ELECTROMECHANICAL_SCHEMA: &str = "lob.board-electromechanical.v1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardElectromechanicalArtifact {
    pub schema: String,
    pub board_id: String,
    pub board_revision: BoardRevision,
    /// Closed board perimeter in the XY plane. The final point is implicitly
    /// connected to the first and must not be repeated.
    pub outline_mm: Vec<Point2Mm>,
    pub thickness_mm: f64,
    pub datums: Vec<BoardDatum>,
    pub mounting_holes: Vec<MechanicalMountingHole>,
    pub connectors: Vec<ConnectorInterface>,
    pub envelopes: Vec<BoardEnvelope>,
    pub mass_properties: BoardMassProperties,
    pub thermal_power: ThermalPowerMetadata,
    pub provenance: ContractProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardRevision {
    pub design: String,
    pub variant: String,
}

/// Mechanical consumer's exact binding to a produced board configuration.
/// Any revision or variant change requires deliberate re-acceptance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardCompatibility {
    pub board_id: String,
    pub design_revision: String,
    pub variant: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point2Mm {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point3Mm {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitVector3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardDatum {
    pub id: String,
    pub origin_mm: Point3Mm,
    pub x_axis: UnitVector3,
    pub y_axis: UnitVector3,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MechanicalMountingHole {
    pub id: String,
    pub datum_id: String,
    pub center_mm: Point2Mm,
    pub finished_diameter_mm: f64,
    pub diameter_tolerance_mm: f64,
    pub position_tolerance_mm: f64,
    pub plated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogIdentity {
    pub manufacturer: String,
    pub part_number: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorInterface {
    pub id: String,
    pub reference_designator: String,
    /// Stable mating interface family, independent of a supplier SKU.
    pub interface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_identity: Option<CatalogIdentity>,
    pub datum_id: String,
    pub location_mm: Point3Mm,
    /// Unit vector pointing away from the board along the mate insertion path.
    pub mating_direction: UnitVector3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    Height,
    Antenna,
    Cable,
    Service,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardEnvelope {
    pub id: String,
    pub kind: EnvelopeKind,
    pub datum_id: String,
    pub footprint_mm: Vec<Point2Mm>,
    pub z_min_mm: f64,
    pub z_max_mm: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardMassProperties {
    pub mass_g: f64,
    pub center_of_gravity_mm: Point3Mm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThermalPowerMetadata {
    pub operating_temperature_min_c: f64,
    pub operating_temperature_max_c: f64,
    pub maximum_dissipation_w: f64,
    pub power_inputs: Vec<PowerInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerInput {
    pub id: String,
    pub connector_id: String,
    pub nominal_voltage_v: f64,
    pub maximum_current_a: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractProvenance {
    pub producer: String,
    pub source_artifact: String,
    pub source_revision: String,
    pub generated_by: String,
    pub generated_at: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BoardContractError {
    #[error("unsupported board contract schema {0:?}")]
    Schema(String),
    #[error("board contract field {0} must be present")]
    Missing(&'static str),
    #[error("board contract field {0} must be finite")]
    NonFinite(&'static str),
    #[error("board contract field {0} must be positive")]
    NonPositive(&'static str),
    #[error("board outline or envelope {0:?} is degenerate")]
    DegeneratePolygon(String),
    #[error("duplicate board contract identity {0:?}")]
    DuplicateId(String),
    #[error("board contract references unknown datum {0:?}")]
    UnknownDatum(String),
    #[error("board contract references unknown connector {0:?}")]
    UnknownConnector(String),
    #[error("vector {0:?} is not a unit vector")]
    NotUnitVector(String),
    #[error("datum {0:?} axes are not perpendicular")]
    NonOrthogonalDatum(String),
    #[error("range {0} is reversed")]
    ReversedRange(&'static str),
    #[error("board configuration is incompatible: expected {expected:?}, got {actual:?}")]
    IncompatibleConfiguration { expected: String, actual: String },
}

impl BoardElectromechanicalArtifact {
    pub fn validate(&self) -> Result<(), BoardContractError> {
        if self.schema != BOARD_ELECTROMECHANICAL_SCHEMA {
            return Err(BoardContractError::Schema(self.schema.clone()));
        }
        for (name, value) in [
            ("board_id", self.board_id.as_str()),
            ("board_revision.design", self.board_revision.design.as_str()),
            (
                "board_revision.variant",
                self.board_revision.variant.as_str(),
            ),
            ("provenance.producer", self.provenance.producer.as_str()),
            (
                "provenance.source_artifact",
                self.provenance.source_artifact.as_str(),
            ),
            (
                "provenance.source_revision",
                self.provenance.source_revision.as_str(),
            ),
            (
                "provenance.generated_by",
                self.provenance.generated_by.as_str(),
            ),
            (
                "provenance.generated_at",
                self.provenance.generated_at.as_str(),
            ),
            ("provenance.digest", self.provenance.digest.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(BoardContractError::Missing(name));
            }
        }
        polygon("board-outline", &self.outline_mm)?;
        positive("thickness_mm", self.thickness_mm)?;
        if self.datums.is_empty() {
            return Err(BoardContractError::Missing("datums"));
        }
        let mut ids = BTreeSet::new();
        for datum in &self.datums {
            unique(&mut ids, &datum.id)?;
            point3("datum.origin_mm", datum.origin_mm)?;
            unit(&datum.id, datum.x_axis)?;
            unit(&datum.id, datum.y_axis)?;
            let dot = datum.x_axis.x * datum.y_axis.x
                + datum.x_axis.y * datum.y_axis.y
                + datum.x_axis.z * datum.y_axis.z;
            if dot.abs() > 1e-6 {
                return Err(BoardContractError::NonOrthogonalDatum(datum.id.clone()));
            }
        }
        let datum_ids = self
            .datums
            .iter()
            .map(|datum| datum.id.as_str())
            .collect::<BTreeSet<_>>();
        ids.clear();
        for hole in &self.mounting_holes {
            unique(&mut ids, &hole.id)?;
            known_datum(&datum_ids, &hole.datum_id)?;
            point2("mounting_hole.center_mm", hole.center_mm)?;
            positive(
                "mounting_hole.finished_diameter_mm",
                hole.finished_diameter_mm,
            )?;
            positive(
                "mounting_hole.diameter_tolerance_mm",
                hole.diameter_tolerance_mm,
            )?;
            positive(
                "mounting_hole.position_tolerance_mm",
                hole.position_tolerance_mm,
            )?;
        }
        ids.clear();
        for connector in &self.connectors {
            unique(&mut ids, &connector.id)?;
            for (name, value) in [
                (
                    "connector.reference_designator",
                    connector.reference_designator.as_str(),
                ),
                ("connector.interface", connector.interface.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Err(BoardContractError::Missing(name));
                }
            }
            if let Some(catalog) = &connector.catalog_identity {
                if catalog.manufacturer.trim().is_empty() || catalog.part_number.trim().is_empty() {
                    return Err(BoardContractError::Missing("connector.catalog_identity"));
                }
            }
            known_datum(&datum_ids, &connector.datum_id)?;
            point3("connector.location_mm", connector.location_mm)?;
            unit(&connector.id, connector.mating_direction)?;
        }
        let connector_ids = self
            .connectors
            .iter()
            .map(|item| item.id.as_str())
            .collect::<BTreeSet<_>>();
        ids.clear();
        for envelope in &self.envelopes {
            unique(&mut ids, &envelope.id)?;
            known_datum(&datum_ids, &envelope.datum_id)?;
            polygon(&envelope.id, &envelope.footprint_mm)?;
            finite("envelope.z_min_mm", envelope.z_min_mm)?;
            finite("envelope.z_max_mm", envelope.z_max_mm)?;
            if envelope.z_min_mm > envelope.z_max_mm {
                return Err(BoardContractError::ReversedRange("envelope z"));
            }
            if let Some(connector_id) = &envelope.connector_id {
                if !connector_ids.contains(connector_id.as_str()) {
                    return Err(BoardContractError::UnknownConnector(connector_id.clone()));
                }
            }
        }
        positive("mass_properties.mass_g", self.mass_properties.mass_g)?;
        point3(
            "mass_properties.center_of_gravity_mm",
            self.mass_properties.center_of_gravity_mm,
        )?;
        finite(
            "thermal_power.operating_temperature_min_c",
            self.thermal_power.operating_temperature_min_c,
        )?;
        finite(
            "thermal_power.operating_temperature_max_c",
            self.thermal_power.operating_temperature_max_c,
        )?;
        if self.thermal_power.operating_temperature_min_c
            > self.thermal_power.operating_temperature_max_c
        {
            return Err(BoardContractError::ReversedRange("operating temperature"));
        }
        nonnegative(
            "thermal_power.maximum_dissipation_w",
            self.thermal_power.maximum_dissipation_w,
        )?;
        ids.clear();
        for input in &self.thermal_power.power_inputs {
            unique(&mut ids, &input.id)?;
            if !connector_ids.contains(input.connector_id.as_str()) {
                return Err(BoardContractError::UnknownConnector(
                    input.connector_id.clone(),
                ));
            }
            positive("power_input.nominal_voltage_v", input.nominal_voltage_v)?;
            positive("power_input.maximum_current_a", input.maximum_current_a)?;
        }
        Ok(())
    }

    /// Validates the artifact and exact board/revision/variant expected by a
    /// mechanical consumer. Compatibility never floats across PCB revisions.
    pub fn validate_compatibility(
        &self,
        expected: &BoardCompatibility,
    ) -> Result<(), BoardContractError> {
        self.validate()?;
        let expected_key = format!(
            "{}@{}:{}",
            expected.board_id, expected.design_revision, expected.variant
        );
        let actual_key = format!(
            "{}@{}:{}",
            self.board_id, self.board_revision.design, self.board_revision.variant
        );
        if expected_key == actual_key {
            Ok(())
        } else {
            Err(BoardContractError::IncompatibleConfiguration {
                expected: expected_key,
                actual: actual_key,
            })
        }
    }
}

fn finite(name: &'static str, value: f64) -> Result<(), BoardContractError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(BoardContractError::NonFinite(name))
    }
}

fn positive(name: &'static str, value: f64) -> Result<(), BoardContractError> {
    finite(name, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(BoardContractError::NonPositive(name))
    }
}

fn nonnegative(name: &'static str, value: f64) -> Result<(), BoardContractError> {
    finite(name, value)?;
    if value >= 0.0 {
        Ok(())
    } else {
        Err(BoardContractError::NonPositive(name))
    }
}

fn point2(name: &'static str, point: Point2Mm) -> Result<(), BoardContractError> {
    finite(name, point.x)?;
    finite(name, point.y)
}

fn point3(name: &'static str, point: Point3Mm) -> Result<(), BoardContractError> {
    finite(name, point.x)?;
    finite(name, point.y)?;
    finite(name, point.z)
}

fn unit(id: &str, vector: UnitVector3) -> Result<(), BoardContractError> {
    let magnitude = (vector.x * vector.x + vector.y * vector.y + vector.z * vector.z).sqrt();
    if magnitude.is_finite() && (magnitude - 1.0).abs() <= 1e-6 {
        Ok(())
    } else {
        Err(BoardContractError::NotUnitVector(id.into()))
    }
}

fn unique(ids: &mut BTreeSet<String>, id: &str) -> Result<(), BoardContractError> {
    if id.trim().is_empty() {
        return Err(BoardContractError::Missing("id"));
    }
    if ids.insert(id.into()) {
        Ok(())
    } else {
        Err(BoardContractError::DuplicateId(id.into()))
    }
}

fn known_datum(datums: &BTreeSet<&str>, id: &str) -> Result<(), BoardContractError> {
    if datums.contains(id) {
        Ok(())
    } else {
        Err(BoardContractError::UnknownDatum(id.into()))
    }
}

fn polygon(id: &str, points: &[Point2Mm]) -> Result<(), BoardContractError> {
    if points.len() < 3 {
        return Err(BoardContractError::DegeneratePolygon(id.into()));
    }
    for point in points {
        point2("polygon point", *point)?;
    }
    let twice_area = points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let next = points[(index + 1) % points.len()];
            point.x * next.y - next.x * point.y
        })
        .sum::<f64>();
    if twice_area.abs() <= 1e-9 {
        Err(BoardContractError::DegeneratePolygon(id.into()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> BoardElectromechanicalArtifact {
        serde_json::from_str(include_str!("../fixtures/board-electromechanical-v1.json")).unwrap()
    }

    #[test]
    fn strict_fixture_round_trips_and_validates() {
        let artifact = fixture();
        artifact.validate().unwrap();
        let encoded = serde_json::to_value(&artifact).unwrap();
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/board-electromechanical-v1.json"))
                .unwrap();
        assert_eq!(encoded, expected);
    }

    #[test]
    fn unknown_fields_and_versions_fail_closed() {
        let mut value = serde_json::to_value(fixture()).unwrap();
        value["guessable_pcb_fact"] = true.into();
        assert!(serde_json::from_value::<BoardElectromechanicalArtifact>(value).is_err());

        let mut artifact = fixture();
        artifact.schema = "lob.board-electromechanical.v2".into();
        assert!(matches!(
            artifact.validate(),
            Err(BoardContractError::Schema(_))
        ));
    }

    #[test]
    fn dangling_interfaces_and_bad_geometry_fail_validation() {
        let mut artifact = fixture();
        artifact.envelopes[0].connector_id = Some("missing".into());
        assert!(matches!(
            artifact.validate(),
            Err(BoardContractError::UnknownConnector(_))
        ));

        let mut artifact = fixture();
        artifact.datums[0].y_axis = artifact.datums[0].x_axis;
        assert!(matches!(
            artifact.validate(),
            Err(BoardContractError::NonOrthogonalDatum(_))
        ));

        let mut artifact = fixture();
        artifact.outline_mm.truncate(2);
        assert!(matches!(
            artifact.validate(),
            Err(BoardContractError::DegeneratePolygon(_))
        ));
    }

    #[test]
    fn consumers_bind_exact_board_revision_and_variant() {
        let artifact = fixture();
        artifact
            .validate_compatibility(&BoardCompatibility {
                board_id: "flight-computer".into(),
                design_revision: "A".into(),
                variant: "flight".into(),
            })
            .unwrap();
        assert!(matches!(
            artifact.validate_compatibility(&BoardCompatibility {
                board_id: "flight-computer".into(),
                design_revision: "B".into(),
                variant: "flight".into(),
            }),
            Err(BoardContractError::IncompatibleConfiguration { .. })
        ));
    }
}
