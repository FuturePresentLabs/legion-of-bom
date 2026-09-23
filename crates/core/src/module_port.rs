//! Shared module port vocabulary.
//!
//! A "voice" is a module role, not an electrical output. What other stages need
//! to route, validate, document, or expose to firmware is a set of typed ports:
//! audio/CV/gate/clock inputs and outputs, controls, normalled sources, and
//! optional expander-bus signals. This module is that small read model.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModuleRole {
    Voice,
    Controller,
    Utility,
    Analog,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortDirection {
    Input,
    Output,
    Bidirectional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortKind {
    Audio,
    Cv,
    Gate,
    Clock,
    Trigger,
    Strike,
    Control,
    Reference,
    Power,
    Ground,
    Bus,
    Data,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortRange {
    pub min: f64,
    pub max: f64,
    pub unit: String,
}

impl PortRange {
    pub fn volts(min: f64, max: f64) -> Self {
        Self {
            min,
            max,
            unit: "V".into(),
        }
    }

    pub fn normalized() -> Self {
        Self {
            min: 0.0,
            max: 1.0,
            unit: "norm".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpanderBinding {
    pub bus: String,
    pub signal: String,
    pub required: bool,
}

impl ExpanderBinding {
    pub fn optional(bus: impl Into<String>, signal: impl Into<String>) -> Self {
        Self {
            bus: bus.into(),
            signal: signal.into(),
            required: false,
        }
    }

    pub fn required(bus: impl Into<String>, signal: impl Into<String>) -> Self {
        Self {
            bus: bus.into(),
            signal: signal.into(),
            required: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortExposure {
    pub panel: bool,
    pub internal: bool,
    pub expander: Option<ExpanderBinding>,
}

impl PortExposure {
    pub fn panel() -> Self {
        Self {
            panel: true,
            internal: false,
            expander: None,
        }
    }

    pub fn internal() -> Self {
        Self {
            panel: false,
            internal: true,
            expander: None,
        }
    }

    pub fn expander(binding: ExpanderBinding) -> Self {
        Self {
            panel: false,
            internal: false,
            expander: Some(binding),
        }
    }

    pub fn panel_and_expander(binding: ExpanderBinding) -> Self {
        Self {
            panel: true,
            internal: false,
            expander: Some(binding),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NormalledSource {
    Ground,
    Volts(f64),
    Internal(String),
    Expander { bus: String, signal: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlSource {
    PanelKnob {
        default: f64,
    },
    CvInput {
        normalled: Option<NormalledSource>,
    },
    Internal {
        signal: String,
    },
    Expander {
        bus: String,
        signal: String,
        normalled: Option<NormalledSource>,
    },
}

impl ControlSource {
    pub fn panel_knob(default: f64) -> Self {
        Self::PanelKnob { default }
    }

    pub fn cv_input(normalled: Option<NormalledSource>) -> Self {
        Self::CvInput { normalled }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlTransform {
    Direct,
    Attenuator { max_gain: f64 },
    Attenuverter { max_abs_gain: f64 },
    Bias { volts: f64 },
    Scale { gain: f64, offset: f64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlPath {
    pub source: ControlSource,
    pub transform: ControlTransform,
}

impl ControlPath {
    pub fn new(source: ControlSource, transform: ControlTransform) -> Self {
        Self { source, transform }
    }

    pub fn panel_knob(default: f64) -> Self {
        Self::new(ControlSource::panel_knob(default), ControlTransform::Direct)
    }

    pub fn attenuverted_cv(max_abs_gain: f64, normalled: NormalledSource) -> Self {
        Self::new(
            ControlSource::cv_input(Some(normalled)),
            ControlTransform::Attenuverter { max_abs_gain },
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlCombine {
    Direct,
    Sum,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortConditioning {
    pub combine: ControlCombine,
    pub paths: Vec<ControlPath>,
}

impl PortConditioning {
    pub fn direct(path: ControlPath) -> Self {
        Self {
            combine: ControlCombine::Direct,
            paths: vec![path],
        }
    }

    pub fn sum(paths: Vec<ControlPath>) -> Self {
        Self {
            combine: ControlCombine::Sum,
            paths,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModulePort {
    pub name: String,
    pub direction: PortDirection,
    pub kind: PortKind,
    pub exposure: PortExposure,
    pub range: Option<PortRange>,
    pub conditioning: Option<PortConditioning>,
}

impl ModulePort {
    pub fn new(
        name: impl Into<String>,
        direction: PortDirection,
        kind: PortKind,
        exposure: PortExposure,
    ) -> Self {
        Self {
            name: name.into(),
            direction,
            kind,
            exposure,
            range: None,
            conditioning: None,
        }
    }

    pub fn panel_input(name: impl Into<String>, kind: PortKind) -> Self {
        Self::new(name, PortDirection::Input, kind, PortExposure::panel())
    }

    pub fn panel_output(name: impl Into<String>, kind: PortKind) -> Self {
        Self::new(name, PortDirection::Output, kind, PortExposure::panel())
    }

    pub fn with_range(mut self, range: PortRange) -> Self {
        self.range = Some(range);
        self
    }

    pub fn with_conditioning(mut self, conditioning: PortConditioning) -> Self {
        self.conditioning = Some(conditioning);
        self
    }

    pub fn with_expander(mut self, binding: ExpanderBinding) -> Self {
        self.exposure.expander = Some(binding);
        self
    }

    pub fn expander_only(
        name: impl Into<String>,
        direction: PortDirection,
        kind: PortKind,
        binding: ExpanderBinding,
    ) -> Self {
        Self::new(name, direction, kind, PortExposure::expander(binding))
    }

    pub fn requires_expander(&self) -> bool {
        self.exposure
            .expander
            .as_ref()
            .is_some_and(|binding| binding.required)
    }

    pub fn is_expander_only(&self) -> bool {
        self.exposure.expander.is_some() && !self.exposure.panel && !self.exposure.internal
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleInterface {
    pub name: String,
    pub role: ModuleRole,
    pub ports: Vec<ModulePort>,
}

impl ModuleInterface {
    pub fn new(name: impl Into<String>, role: ModuleRole) -> Self {
        Self {
            name: name.into(),
            role,
            ports: Vec::new(),
        }
    }

    pub fn with_port(mut self, port: ModulePort) -> Self {
        self.ports.push(port);
        self
    }

    pub fn port(&self, name: &str) -> Option<&ModulePort> {
        self.ports.iter().find(|port| port.name == name)
    }

    pub fn ports_by_kind(&self, kind: PortKind) -> impl Iterator<Item = &ModulePort> {
        self.ports.iter().filter(move |port| port.kind == kind)
    }

    pub fn standalone_ports(&self) -> impl Iterator<Item = &ModulePort> {
        self.ports.iter().filter(|port| !port.is_expander_only())
    }

    pub fn requires_expander(&self) -> bool {
        self.ports.iter().any(ModulePort::requires_expander)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dsp_voice_has_typed_inputs_outputs_not_voice_outputs() {
        let voice = ModuleInterface::new("handpan-core", ModuleRole::Voice)
            .with_port(
                ModulePort::panel_input("V/OCT", PortKind::Cv)
                    .with_range(PortRange::volts(-5.0, 5.0)),
            )
            .with_port(ModulePort::panel_input("STRIKE", PortKind::Strike))
            .with_port(
                ModulePort::panel_input("VELOCITY", PortKind::Cv)
                    .with_range(PortRange::volts(0.0, 10.0))
                    .with_conditioning(PortConditioning::sum(vec![
                        ControlPath::panel_knob(0.8),
                        ControlPath::attenuverted_cv(1.0, NormalledSource::Volts(0.0)),
                    ]))
                    .with_expander(ExpanderBinding::optional("HANDPAN", "VELOCITY")),
            )
            .with_port(ModulePort::expander_only(
                "CLOCK",
                PortDirection::Input,
                PortKind::Clock,
                ExpanderBinding::optional("HANDPAN", "CLOCK"),
            ))
            .with_port(ModulePort::panel_output("LEFT", PortKind::Audio))
            .with_port(ModulePort::panel_output("RIGHT", PortKind::Audio));

        assert_eq!(voice.role, ModuleRole::Voice);
        assert_eq!(voice.ports_by_kind(PortKind::Audio).count(), 2);
        assert!(voice
            .ports_by_kind(PortKind::Audio)
            .all(|port| port.direction == PortDirection::Output));
        assert_eq!(
            voice.port("V/OCT").map(|port| port.direction),
            Some(PortDirection::Input)
        );
        assert!(!voice.requires_expander());
        assert!(voice.standalone_ports().all(|port| port.name != "CLOCK"));

        let velocity = voice.port("VELOCITY").unwrap();
        let conditioning = velocity.conditioning.as_ref().unwrap();
        assert_eq!(conditioning.combine, ControlCombine::Sum);
        assert!(conditioning.paths.iter().any(|path| {
            matches!(
                path.transform,
                ControlTransform::Attenuverter { max_abs_gain: 1.0 }
            )
        }));
    }

    #[test]
    fn a_brain_can_consume_clock_and_drive_gate_and_voct() {
        let brain = ModuleInterface::new("handpan-brain", ModuleRole::Controller)
            .with_port(ModulePort::panel_input("CLOCK", PortKind::Clock))
            .with_port(
                ModulePort::panel_output("GATE", PortKind::Gate)
                    .with_expander(ExpanderBinding::optional("HANDPAN", "GATE")),
            )
            .with_port(
                ModulePort::panel_output("V/OCT", PortKind::Cv)
                    .with_range(PortRange::volts(-5.0, 5.0))
                    .with_expander(ExpanderBinding::optional("HANDPAN", "V/OCT")),
            );

        assert_eq!(brain.port("CLOCK").unwrap().direction, PortDirection::Input);
        assert_eq!(brain.port("GATE").unwrap().direction, PortDirection::Output);
        assert!(!brain.requires_expander());
    }

    #[test]
    fn analog_modules_share_the_same_conditioned_cv_vocabulary() {
        let analog_filter = ModuleInterface::new("state-variable-filter", ModuleRole::Analog)
            .with_port(ModulePort::panel_input("IN", PortKind::Audio))
            .with_port(ModulePort::panel_output("OUT", PortKind::Audio))
            .with_port(
                ModulePort::panel_input("CUTOFF", PortKind::Control)
                    .with_range(PortRange::normalized())
                    .with_conditioning(PortConditioning::sum(vec![
                        ControlPath::panel_knob(0.5),
                        ControlPath::new(
                            ControlSource::cv_input(Some(NormalledSource::Ground)),
                            ControlTransform::Attenuverter { max_abs_gain: 1.0 },
                        ),
                        ControlPath::new(
                            ControlSource::Internal {
                                signal: "trim".into(),
                            },
                            ControlTransform::Bias { volts: 0.25 },
                        ),
                    ]))
                    .with_expander(ExpanderBinding::optional("FILTER", "CUTOFF_CV")),
            );

        assert_eq!(analog_filter.role, ModuleRole::Analog);
        assert_eq!(
            analog_filter.port("CUTOFF").unwrap().direction,
            PortDirection::Input
        );
        assert!(!analog_filter.requires_expander());
        let cutoff = analog_filter.port("CUTOFF").unwrap();
        let paths = &cutoff.conditioning.as_ref().unwrap().paths;
        assert!(paths
            .iter()
            .any(|path| matches!(path.transform, ControlTransform::Bias { volts: 0.25 })));
        assert!(paths.iter().any(|path| {
            matches!(
                path.transform,
                ControlTransform::Attenuverter { max_abs_gain: 1.0 }
            )
        }));
    }
}
