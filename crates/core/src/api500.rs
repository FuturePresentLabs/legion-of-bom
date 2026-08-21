//! API/VPR 500-series card-edge vocabulary and validation.
//!
//! The 500-series backplane is not a Eurorack power header. It is the module's
//! primary external interface: balanced line I/O, chassis/common separation,
//! +/-16 V rails, +48 V phantom, gain trim, and stereo link all arrive on a
//! 15-position EDAC/EDA-306 style card edge. Keeping those pins typed here lets
//! panel, board, and carrier code share one contract.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::model::PinRef;
use crate::module_port::{ModulePort, PortDirection, PortKind};
use crate::source::CircuitSource;
use crate::stage::{Finding, StageOutcome};

pub const API500_STAGE: &str = "api500";
pub const API500_CARD_EDGE_FOOTPRINT: &str = "Cards:EDA_306";
pub const API500_CARD_EDGE_VALUE: &str = "EDA 306";
pub const API500_PIN_COUNT: usize = 15;
pub const API500_PIN_PITCH_MM: f64 = 3.9624;
pub const API500_PLUS_16_LIMIT_MA: f64 = 130.0;
pub const API500_MINUS_16_LIMIT_MA: f64 = 130.0;
pub const API500_PHANTOM_LIMIT_MA: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Api500Pin {
    ChassisGround,
    OutputPlus4Hot,
    OutputMinus2Hot,
    OutputCold,
    AudioCommon,
    StereoLink,
    InputMinus2Cold,
    InputPlus4Cold,
    InputMinus2Hot,
    InputPlus4Hot,
    GainTrim,
    Plus16V,
    PowerCommon,
    Minus16V,
    Phantom48V,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Api500PinClass {
    ChassisGround,
    AudioCommon,
    PowerCommon,
    AudioInput,
    AudioOutput,
    Control,
    PowerRail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Api500PinSpec {
    pub pin: Api500Pin,
    pub number: u8,
    pub net: &'static str,
    pub label: &'static str,
    pub class: Api500PinClass,
}

pub const API500_PINS: [Api500PinSpec; API500_PIN_COUNT] = [
    Api500PinSpec {
        pin: Api500Pin::ChassisGround,
        number: 1,
        net: "CHASSIS",
        label: "CHASSIS",
        class: Api500PinClass::ChassisGround,
    },
    Api500PinSpec {
        pin: Api500Pin::OutputPlus4Hot,
        number: 2,
        net: "+OUT+4",
        label: "+OUT+4",
        class: Api500PinClass::AudioOutput,
    },
    Api500PinSpec {
        pin: Api500Pin::OutputMinus2Hot,
        number: 3,
        net: "+OUT-2",
        label: "+OUT-2",
        class: Api500PinClass::AudioOutput,
    },
    Api500PinSpec {
        pin: Api500Pin::OutputCold,
        number: 4,
        net: "-OUT",
        label: "-OUT",
        class: Api500PinClass::AudioOutput,
    },
    Api500PinSpec {
        pin: Api500Pin::AudioCommon,
        number: 5,
        net: "AGND",
        label: "AGND",
        class: Api500PinClass::AudioCommon,
    },
    Api500PinSpec {
        pin: Api500Pin::StereoLink,
        number: 6,
        net: "SC_LINK",
        label: "SC LINK",
        class: Api500PinClass::Control,
    },
    Api500PinSpec {
        pin: Api500Pin::InputMinus2Cold,
        number: 7,
        net: "-IN-2",
        label: "-IN-2",
        class: Api500PinClass::AudioInput,
    },
    Api500PinSpec {
        pin: Api500Pin::InputPlus4Cold,
        number: 8,
        net: "-IN+4",
        label: "-IN+4",
        class: Api500PinClass::AudioInput,
    },
    Api500PinSpec {
        pin: Api500Pin::InputMinus2Hot,
        number: 9,
        net: "+IN-2",
        label: "+IN-2",
        class: Api500PinClass::AudioInput,
    },
    Api500PinSpec {
        pin: Api500Pin::InputPlus4Hot,
        number: 10,
        net: "+IN+4",
        label: "+IN+4",
        class: Api500PinClass::AudioInput,
    },
    Api500PinSpec {
        pin: Api500Pin::GainTrim,
        number: 11,
        net: "GAIN_ADJ",
        label: "GAIN TRIM RESISTOR",
        class: Api500PinClass::Control,
    },
    Api500PinSpec {
        pin: Api500Pin::Plus16V,
        number: 12,
        net: "+16V",
        label: "+16VDC",
        class: Api500PinClass::PowerRail,
    },
    Api500PinSpec {
        pin: Api500Pin::PowerCommon,
        number: 13,
        net: "GND",
        label: "PWRGND",
        class: Api500PinClass::PowerCommon,
    },
    Api500PinSpec {
        pin: Api500Pin::Minus16V,
        number: 14,
        net: "-16V",
        label: "-16VDC",
        class: Api500PinClass::PowerRail,
    },
    Api500PinSpec {
        pin: Api500Pin::Phantom48V,
        number: 15,
        net: "+48V",
        label: "+48VDC",
        class: Api500PinClass::PowerRail,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Api500PowerBudget {
    pub plus_16_ma: f64,
    pub minus_16_ma: f64,
    pub phantom_48_ma: f64,
}

impl Api500PowerBudget {
    pub fn new(plus_16_ma: f64, minus_16_ma: f64, phantom_48_ma: f64) -> Self {
        Self {
            plus_16_ma,
            minus_16_ma,
            phantom_48_ma,
        }
    }

    pub fn within_vpr_limits(&self) -> bool {
        self.plus_16_ma <= API500_PLUS_16_LIMIT_MA
            && self.minus_16_ma <= API500_MINUS_16_LIMIT_MA
            && self.phantom_48_ma <= API500_PHANTOM_LIMIT_MA
    }

    pub fn findings(&self) -> Vec<Finding> {
        let mut findings = Vec::new();
        if self.plus_16_ma > API500_PLUS_16_LIMIT_MA {
            findings.push(Finding::error(format!(
                "+16V budget {:.1} mA exceeds 500-series limit {:.1} mA",
                self.plus_16_ma, API500_PLUS_16_LIMIT_MA
            )));
        }
        if self.minus_16_ma > API500_MINUS_16_LIMIT_MA {
            findings.push(Finding::error(format!(
                "-16V budget {:.1} mA exceeds 500-series limit {:.1} mA",
                self.minus_16_ma, API500_MINUS_16_LIMIT_MA
            )));
        }
        if self.phantom_48_ma > API500_PHANTOM_LIMIT_MA {
            findings.push(Finding::error(format!(
                "+48V budget {:.1} mA exceeds 500-series limit {:.1} mA",
                self.phantom_48_ma, API500_PHANTOM_LIMIT_MA
            )));
        }
        findings
    }
}

impl Default for Api500PowerBudget {
    fn default() -> Self {
        Self::new(0.0, 0.0, 0.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Api500PortBinding {
    pub port: String,
    pub pin: Api500Pin,
}

impl Api500PortBinding {
    pub fn new(port: impl Into<String>, pin: Api500Pin) -> Self {
        Self {
            port: port.into(),
            pin,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Api500ValidationOptions {
    pub allow_chassis_audio_common_link: bool,
    pub allow_audio_power_common_link: bool,
}

pub fn pin_spec(pin: Api500Pin) -> &'static Api500PinSpec {
    API500_PINS
        .iter()
        .find(|spec| spec.pin == pin)
        .expect("every Api500Pin is represented in API500_PINS")
}

pub fn pin_by_number(number: u8) -> Option<Api500Pin> {
    API500_PINS
        .iter()
        .find(|spec| spec.number == number)
        .map(|spec| spec.pin)
}

pub fn pin_by_net(net: &str) -> Option<Api500Pin> {
    API500_PINS
        .iter()
        .find(|spec| spec.net.eq_ignore_ascii_case(net.trim_start_matches('/')))
        .map(|spec| spec.pin)
}

pub fn pins_by_class(class: Api500PinClass) -> impl Iterator<Item = &'static Api500PinSpec> {
    API500_PINS.iter().filter(move |spec| spec.class == class)
}

pub fn card_edge_pin_ref(refdes: impl Into<crate::model::RefDes>, pin: Api500Pin) -> PinRef {
    PinRef::new(refdes, pin_spec(pin).number.to_string())
}

pub fn validate_api500_bindings(
    ports: &[ModulePort],
    bindings: &[Api500PortBinding],
    budget: Api500PowerBudget,
) -> StageOutcome {
    let mut outcome = StageOutcome::passed(API500_STAGE);
    let port_map: HashMap<&str, &ModulePort> = ports
        .iter()
        .map(|port| (port.name.as_str(), port))
        .collect();
    let mut bound_pins = BTreeSet::new();

    for binding in bindings {
        let Some(port) = port_map.get(binding.port.as_str()) else {
            outcome = outcome.with(Finding::error(format!(
                "API 500 binding references unknown port {}",
                binding.port
            )));
            continue;
        };
        bound_pins.insert(binding.pin);
        if !port_compatible_with_pin(port, binding.pin) {
            outcome = outcome.with(Finding::error(format!(
                "port {} ({:?} {:?}) is not compatible with API 500 pin {} {}",
                port.name,
                port.direction,
                port.kind,
                pin_spec(binding.pin).number,
                pin_spec(binding.pin).label
            )));
        }
    }

    for required in [
        Api500Pin::AudioCommon,
        Api500Pin::Plus16V,
        Api500Pin::PowerCommon,
        Api500Pin::Minus16V,
    ] {
        if !bound_pins.contains(&required) {
            outcome = outcome.with(Finding::error(format!(
                "API 500 binding set is missing required pin {} {}",
                pin_spec(required).number,
                pin_spec(required).label
            )));
        }
    }

    for finding in budget.findings() {
        outcome = outcome.with(finding);
    }

    outcome.with(Finding::info(format!(
        "API 500 bindings: {} port(s), {} card-edge pin(s)",
        bindings.len(),
        bound_pins.len()
    )))
}

pub fn validate_api500_circuit(circuit: &dyn CircuitSource) -> StageOutcome {
    validate_api500_circuit_with(circuit, &Api500ValidationOptions::default())
}

pub fn validate_api500_circuit_with(
    circuit: &dyn CircuitSource,
    options: &Api500ValidationOptions,
) -> StageOutcome {
    let edge_parts: Vec<_> = circuit
        .parts()
        .iter()
        .filter(|part| is_api500_card_edge(part))
        .collect();
    if edge_parts.is_empty() {
        return StageOutcome::passed(API500_STAGE)
            .with(Finding::info("no API 500 card-edge connector found"));
    }

    let mut outcome = StageOutcome::passed(API500_STAGE);
    for part in &edge_parts {
        let pins_by_number = edge_pin_nets(circuit, &part.refdes.0);
        for spec in &API500_PINS {
            match pins_by_number.get(&spec.number) {
                Some(net) if pin_net_allowed(spec.pin, net, options) => {}
                Some(net) => {
                    outcome = outcome.with(Finding::error(format!(
                        "{} pin {} should be {} but is connected to {}",
                        part.refdes, spec.number, spec.net, net
                    )));
                }
                None if optional_unconnected_pin(spec.pin) => {}
                None => {
                    outcome = outcome.with(Finding::error(format!(
                        "{} pin {} {} is not connected",
                        part.refdes, spec.number, spec.label
                    )));
                }
            }
        }
    }

    for net in circuit.nets() {
        let classes = api500_classes_on_net(net);
        if classes.contains(&Api500PinClass::ChassisGround)
            && classes.contains(&Api500PinClass::AudioCommon)
            && !options.allow_chassis_audio_common_link
        {
            outcome = outcome.with(Finding::error(format!(
                "net {} shorts API 500 chassis ground to audio common",
                net.name
            )));
        }
        if classes.contains(&Api500PinClass::AudioCommon)
            && classes.contains(&Api500PinClass::PowerCommon)
            && !options.allow_audio_power_common_link
        {
            outcome = outcome.with(Finding::error(format!(
                "net {} shorts API 500 audio common to power common",
                net.name
            )));
        }
    }

    outcome.with(Finding::info(format!(
        "API 500 validation: {} card-edge connector(s)",
        edge_parts.len()
    )))
}

fn port_compatible_with_pin(port: &ModulePort, pin: Api500Pin) -> bool {
    use Api500Pin::*;
    match pin {
        ChassisGround => port.kind == PortKind::Ground,
        AudioCommon | PowerCommon => matches!(port.kind, PortKind::Ground | PortKind::Reference),
        Plus16V => port.kind == PortKind::Power && port.name.eq_ignore_ascii_case("+16V"),
        Minus16V => port.kind == PortKind::Power && port.name.eq_ignore_ascii_case("-16V"),
        Phantom48V => port.kind == PortKind::Power && port.name.eq_ignore_ascii_case("+48V"),
        OutputPlus4Hot | OutputMinus2Hot | OutputCold => {
            port.kind == PortKind::Audio && port.direction == PortDirection::Output
        }
        InputMinus2Cold | InputPlus4Cold | InputMinus2Hot | InputPlus4Hot => {
            port.kind == PortKind::Audio && port.direction == PortDirection::Input
        }
        StereoLink | GainTrim => {
            matches!(port.kind, PortKind::Control | PortKind::Bus | PortKind::Cv)
        }
    }
}

fn optional_unconnected_pin(pin: Api500Pin) -> bool {
    matches!(
        pin,
        Api500Pin::StereoLink
            | Api500Pin::GainTrim
            | Api500Pin::OutputMinus2Hot
            | Api500Pin::InputMinus2Cold
            | Api500Pin::InputMinus2Hot
    )
}

fn is_api500_card_edge(part: &crate::model::Part) -> bool {
    part.footprint
        .as_deref()
        .is_some_and(|fp| fp.eq_ignore_ascii_case(API500_CARD_EDGE_FOOTPRINT))
        || part
            .value
            .trim()
            .eq_ignore_ascii_case(API500_CARD_EDGE_VALUE)
}

fn edge_pin_nets(circuit: &dyn CircuitSource, refdes: &str) -> BTreeMap<u8, String> {
    let mut pins = BTreeMap::new();
    for net in circuit.nets() {
        for pin in &net.pins {
            if pin.refdes.0 == refdes {
                if let Ok(number) = pin.pin.parse::<u8>() {
                    pins.insert(number, net.name.trim_start_matches('/').to_string());
                }
            }
        }
    }
    pins
}

fn api500_classes_on_net(net: &crate::model::Net) -> BTreeSet<Api500PinClass> {
    net.pins
        .iter()
        .filter_map(|pin| pin.pin.parse::<u8>().ok())
        .filter_map(pin_by_number)
        .map(|pin| pin_spec(pin).class)
        .collect()
}

fn net_name_matches(actual: &str, expected: &str) -> bool {
    actual
        .trim_start_matches('/')
        .eq_ignore_ascii_case(expected.trim_start_matches('/'))
}

fn pin_net_allowed(pin: Api500Pin, net: &str, options: &Api500ValidationOptions) -> bool {
    if net_name_matches(net, pin_spec(pin).net) {
        return true;
    }
    match pin {
        Api500Pin::ChassisGround if options.allow_chassis_audio_common_link => {
            net_name_matches(net, pin_spec(Api500Pin::AudioCommon).net)
        }
        Api500Pin::AudioCommon if options.allow_chassis_audio_common_link => {
            net_name_matches(net, pin_spec(Api500Pin::ChassisGround).net)
        }
        Api500Pin::AudioCommon if options.allow_audio_power_common_link => {
            net_name_matches(net, pin_spec(Api500Pin::PowerCommon).net)
        }
        Api500Pin::PowerCommon if options.allow_audio_power_common_link => {
            net_name_matches(net, pin_spec(Api500Pin::AudioCommon).net)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part};
    use crate::module_port::{ModulePort, PortExposure};

    fn card_edge() -> Part {
        Part::new("J1", API500_CARD_EDGE_VALUE).with_footprint(API500_CARD_EDGE_FOOTPRINT)
    }

    fn valid_circuit() -> Circuit {
        let mut circuit = Circuit::new("api500_fixture");
        circuit.parts.push(card_edge());
        for spec in &API500_PINS {
            if optional_unconnected_pin(spec.pin) {
                continue;
            }
            circuit
                .nets
                .push(Net::new(spec.net, vec![card_edge_pin_ref("J1", spec.pin)]));
        }
        circuit
    }

    fn minimal_ports() -> Vec<ModulePort> {
        vec![
            ModulePort::new(
                "CHASSIS",
                PortDirection::Bidirectional,
                PortKind::Ground,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "OUT+",
                PortDirection::Output,
                PortKind::Audio,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "OUT-",
                PortDirection::Output,
                PortKind::Audio,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "IN+",
                PortDirection::Input,
                PortKind::Audio,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "IN-",
                PortDirection::Input,
                PortKind::Audio,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "AGND",
                PortDirection::Bidirectional,
                PortKind::Reference,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "PWRGND",
                PortDirection::Bidirectional,
                PortKind::Ground,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "+16V",
                PortDirection::Input,
                PortKind::Power,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "-16V",
                PortDirection::Input,
                PortKind::Power,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "+48V",
                PortDirection::Input,
                PortKind::Power,
                PortExposure::internal(),
            ),
            ModulePort::new(
                "LINK",
                PortDirection::Bidirectional,
                PortKind::Bus,
                PortExposure::internal(),
            ),
        ]
    }

    #[test]
    fn card_edge_profile_matches_the_api_and_kicad_template_pinout() {
        assert_eq!(pin_spec(Api500Pin::ChassisGround).number, 1);
        assert_eq!(pin_spec(Api500Pin::OutputPlus4Hot).net, "+OUT+4");
        assert_eq!(pin_spec(Api500Pin::AudioCommon).net, "AGND");
        assert_eq!(pin_spec(Api500Pin::Plus16V).number, 12);
        assert_eq!(pin_spec(Api500Pin::PowerCommon).net, "GND");
        assert_eq!(pin_spec(Api500Pin::Phantom48V).number, 15);
        assert_eq!(pin_by_net("/+48V"), Some(Api500Pin::Phantom48V));
        assert_eq!(pins_by_class(Api500PinClass::AudioInput).count(), 4);
    }

    #[test]
    fn module_ports_bind_to_typed_card_edge_pins() {
        let ports = minimal_ports();
        let bindings = vec![
            Api500PortBinding::new("CHASSIS", Api500Pin::ChassisGround),
            Api500PortBinding::new("OUT+", Api500Pin::OutputPlus4Hot),
            Api500PortBinding::new("OUT-", Api500Pin::OutputCold),
            Api500PortBinding::new("IN+", Api500Pin::InputPlus4Hot),
            Api500PortBinding::new("IN-", Api500Pin::InputPlus4Cold),
            Api500PortBinding::new("AGND", Api500Pin::AudioCommon),
            Api500PortBinding::new("PWRGND", Api500Pin::PowerCommon),
            Api500PortBinding::new("+16V", Api500Pin::Plus16V),
            Api500PortBinding::new("-16V", Api500Pin::Minus16V),
            Api500PortBinding::new("+48V", Api500Pin::Phantom48V),
            Api500PortBinding::new("LINK", Api500Pin::StereoLink),
        ];

        let outcome =
            validate_api500_bindings(&ports, &bindings, Api500PowerBudget::new(40.0, 35.0, 2.0));

        assert!(outcome.passed, "{outcome:?}");
    }

    #[test]
    fn binding_validation_rejects_swapped_rails_and_missing_common() {
        let ports = minimal_ports();
        let bindings = vec![
            Api500PortBinding::new("OUT+", Api500Pin::OutputPlus4Hot),
            Api500PortBinding::new("PWRGND", Api500Pin::PowerCommon),
            Api500PortBinding::new("+16V", Api500Pin::Minus16V),
            Api500PortBinding::new("-16V", Api500Pin::Plus16V),
        ];

        let outcome =
            validate_api500_bindings(&ports, &bindings, Api500PowerBudget::new(40.0, 35.0, 0.0));

        assert!(!outcome.passed);
        assert!(outcome
            .findings
            .iter()
            .any(|finding| finding.message.contains("not compatible")));
        assert!(outcome
            .findings
            .iter()
            .any(|finding| finding.message.contains("pin 5 AGND")));
    }

    #[test]
    fn power_budget_validation_rejects_over_current() {
        let findings = Api500PowerBudget::new(131.0, 130.0, 5.1).findings();

        assert_eq!(findings.len(), 2);
        assert!(findings
            .iter()
            .any(|finding| finding.message.contains("+16V budget")));
        assert!(findings
            .iter()
            .any(|finding| finding.message.contains("+48V budget")));
    }

    #[test]
    fn circuit_validation_accepts_named_card_edge_nets() {
        let outcome = validate_api500_circuit(&valid_circuit());

        assert!(outcome.passed, "{outcome:?}");
    }

    #[test]
    fn circuit_validation_rejects_swapped_card_edge_rails() {
        let mut circuit = valid_circuit();
        for net in &mut circuit.nets {
            if net.name == "+16V" {
                net.name = "-16V".into();
            } else if net.name == "-16V" {
                net.name = "+16V".into();
            }
        }

        let outcome = validate_api500_circuit(&circuit);

        assert!(!outcome.passed);
        assert!(outcome
            .findings
            .iter()
            .any(|finding| finding.message.contains("pin 12 should be +16V")));
        assert!(outcome
            .findings
            .iter()
            .any(|finding| finding.message.contains("pin 14 should be -16V")));
    }

    #[test]
    fn circuit_validation_rejects_missing_audio_common() {
        let mut circuit = valid_circuit();
        circuit.nets.retain(|net| net.name != "AGND");

        let outcome = validate_api500_circuit(&circuit);

        assert!(!outcome.passed);
        assert!(outcome
            .findings
            .iter()
            .any(|finding| finding.message.contains("pin 5 AGND is not connected")));
    }

    #[test]
    fn circuit_validation_rejects_chassis_audio_common_short_unless_allowed() {
        let mut circuit = valid_circuit();
        circuit.nets.retain(|net| net.name != "AGND");
        circuit
            .nets
            .iter_mut()
            .find(|net| net.name == "CHASSIS")
            .unwrap()
            .pins
            .push(card_edge_pin_ref("J1", Api500Pin::AudioCommon));

        let outcome = validate_api500_circuit(&circuit);
        assert!(!outcome.passed);
        assert!(outcome
            .findings
            .iter()
            .any(|finding| finding.message.contains("chassis ground to audio common")));

        let allowed = validate_api500_circuit_with(
            &circuit,
            &Api500ValidationOptions {
                allow_chassis_audio_common_link: true,
                allow_audio_power_common_link: false,
            },
        );
        assert!(allowed.passed, "{allowed:?}");
    }
}
