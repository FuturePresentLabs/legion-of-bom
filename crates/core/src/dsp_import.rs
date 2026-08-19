//! Import manifests for externally owned DSP instrument cores.
//!
//! Legion should not take ownership of a DSP repository just to make hardware
//! from it. This read model describes the small contract the downstream carrier,
//! firmware, and VCV wrappers need: where the core crate lives, what module roles
//! it can fill, which typed ports each role exposes, and whether any signals
//! participate in an optional expander bus.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::carrier::CarrierPlatform;
use crate::module_port::{ModuleInterface, ModuleRole, PortKind};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct DspImportManifest {
    pub package: DspPackage,
    #[serde(default)]
    pub cores: Vec<DspCore>,
    #[serde(default)]
    pub modules: Vec<DspModule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DspPackage {
    pub name: String,
    pub repo_path: String,
    #[serde(default)]
    pub source_branch: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DspCore {
    pub name: String,
    pub crate_path: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub firmware_targets: Vec<TargetRef>,
    #[serde(default)]
    pub vcv_targets: Vec<TargetRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TargetRef {
    pub path: String,
    #[serde(default)]
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct DspModule {
    pub name: String,
    pub core: String,
    #[serde(default)]
    pub preferred_carrier: Option<CarrierPlatform>,
    #[serde(default)]
    pub firmware_target: Option<String>,
    #[serde(default)]
    pub vcv_target: Option<String>,
    pub interface: ModuleInterface,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DspImportProblem {
    pub subject: String,
    pub problem: &'static str,
}

impl DspImportManifest {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn core(&self, name: &str) -> Option<&DspCore> {
        self.cores.iter().find(|core| core.name == name)
    }

    pub fn module(&self, name: &str) -> Option<&DspModule> {
        self.modules.iter().find(|module| module.name == name)
    }

    pub fn role_modules(&self, role: ModuleRole) -> impl Iterator<Item = &DspModule> {
        self.modules
            .iter()
            .filter(move |module| module.interface.role == role)
    }

    pub fn core_path(&self, repo_root: &Path, core: &DspCore) -> PathBuf {
        repo_root
            .join(&self.package.repo_path)
            .join(&core.crate_path)
    }

    pub fn validation_problems(&self) -> Vec<DspImportProblem> {
        let cores: BTreeSet<&str> = self.cores.iter().map(|core| core.name.as_str()).collect();
        let mut problems = Vec::new();

        for module in &self.modules {
            if !cores.contains(module.core.as_str()) {
                problems.push(DspImportProblem {
                    subject: module.name.clone(),
                    problem: "references an unknown DSP core",
                });
            }
            if module.interface.ports.is_empty() {
                problems.push(DspImportProblem {
                    subject: module.name.clone(),
                    problem: "declares no typed ports",
                });
            }
            if module
                .interface
                .ports
                .iter()
                .any(|port| port.name.trim().is_empty())
            {
                problems.push(DspImportProblem {
                    subject: module.name.clone(),
                    problem: "declares a port with no name",
                });
            }
        }

        problems
    }
}

impl DspModule {
    pub fn is_voice(&self) -> bool {
        self.interface.role == ModuleRole::Voice
    }

    pub fn audio_outputs(&self) -> impl Iterator<Item = &crate::ModulePort> {
        self.interface
            .ports_by_kind(PortKind::Audio)
            .filter(|port| port.direction == crate::PortDirection::Output)
    }

    pub fn remains_standalone_without_expander(&self) -> bool {
        !self.interface.requires_expander() && self.interface.standalone_ports().next().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ControlCombine, ControlSource, ControlTransform, NormalledSource, PortDirection, PortRange,
    };

    const HANDPAN_FAMILY: &str = include_str!("../fixtures/handpan_dsp_manifest.toml");

    fn manifest() -> DspImportManifest {
        let manifest = DspImportManifest::from_toml(HANDPAN_FAMILY).expect("fixture parses");
        assert_eq!(manifest.validation_problems(), Vec::new());
        manifest
    }

    #[test]
    fn handpan_family_fixture_describes_all_imported_cores() {
        let m = manifest();
        let names: BTreeSet<&str> = m.cores.iter().map(|core| core.name.as_str()).collect();

        assert!(names.contains("handpan-core"));
        assert!(names.contains("mallet-core"));
        assert!(names.contains("bowed-core"));
        assert!(names.contains("wind-core"));
        assert_eq!(m.role_modules(ModuleRole::Voice).count(), 4);
    }

    #[test]
    fn voice_manifest_uses_typed_ports_not_voice_outputs() {
        let handpan = manifest().module("handpan-voice").unwrap().clone();

        assert!(handpan.is_voice());
        assert_eq!(handpan.audio_outputs().count(), 2);
        assert_eq!(
            handpan.interface.port("V/OCT").map(|port| port.direction),
            Some(PortDirection::Input)
        );
        assert_eq!(
            handpan
                .interface
                .port("V/OCT")
                .and_then(|port| port.range.as_ref()),
            Some(&PortRange::volts(-5.0, 5.0))
        );
        assert!(handpan.remains_standalone_without_expander());
    }

    #[test]
    fn controls_carry_knob_cv_normalled_and_attenuverter_semantics() {
        let handpan = manifest().module("handpan-voice").unwrap().clone();
        let velocity = handpan.interface.port("VELOCITY").unwrap();
        let conditioning = velocity.conditioning.as_ref().unwrap();

        assert_eq!(conditioning.combine, ControlCombine::Sum);
        assert!(conditioning
            .paths
            .iter()
            .any(|path| matches!(path.source, ControlSource::PanelKnob { default: 0.8 })));
        assert!(conditioning.paths.iter().any(|path| matches!(
            (&path.source, &path.transform),
            (
                ControlSource::CvInput {
                    normalled: Some(NormalledSource::Ground)
                },
                ControlTransform::Attenuverter { max_abs_gain: 1.0 }
            )
        )));
    }

    #[test]
    fn optional_expander_bus_does_not_make_the_voice_depend_on_the_expander() {
        let m = manifest();
        for module in &m.modules {
            assert!(
                module.remains_standalone_without_expander(),
                "{} should have panel-facing standalone ports",
                module.name
            );
        }

        let brain = m.module("handpan-brain").unwrap();
        assert!(!brain.is_voice());
        assert_eq!(
            brain.interface.port("CLOCK").map(|port| port.direction),
            Some(PortDirection::Input)
        );
        assert_eq!(
            brain.interface.port("GATE").map(|port| port.direction),
            Some(PortDirection::Output)
        );
    }
}
