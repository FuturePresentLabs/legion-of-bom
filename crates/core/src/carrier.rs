//! Carrier-board helpers for bought DSP modules / SOMs.
//!
//! A carrier is still just a [`Circuit`](crate::model::Circuit): one part is the
//! sub-board (`LobModule:DAISY_PATCH_SM` / `LobModule:DAISY_SEED2_DFM`), panel hardware is
//! ordinary parts, and nets connect them explicitly. This module only gives that
//! pattern names so a carrier can say "audio input jack to AUDIO_IN_L" instead
//! of hand-spelling the same netlist every time.

use std::fmt;

use serde_json::json;

use crate::model::{Circuit, Net, Part, PinRef, RefDes};
use crate::source::CircuitSource;

pub const DEFAULT_JACK_FOOTPRINT: &str = "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical";
pub const DEFAULT_POT_FOOTPRINT: &str =
    "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical";
pub const DEFAULT_RESISTOR_FOOTPRINT: &str = "Resistor_SMD:R_0805_2012Metric";
pub const DEFAULT_CAPACITOR_FOOTPRINT: &str = "Capacitor_SMD:C_0805_2012Metric";
pub const DEFAULT_DIODE_FOOTPRINT: &str = "Diode_SMD:D_SOD-323";
pub const DEFAULT_OPAMP_FOOTPRINT: &str = "Package_SO:SOIC-8_3.9x4.9mm";
pub const DEFAULT_TESTPOINT_FOOTPRINT: &str = "TestPoint:TestPoint_Pad_D1.5mm";
pub const DEFAULT_EXPANDER_HEADER_FOOTPRINT: &str =
    "Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm_Vertical";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierPlatform {
    PatchSm,
    Seed2Dfm,
}

impl CarrierPlatform {
    pub fn module_value(self) -> &'static str {
        match self {
            CarrierPlatform::PatchSm => "DAISY_PATCH_SM",
            CarrierPlatform::Seed2Dfm => "DAISY_SEED2_DFM",
        }
    }

    pub fn module_footprint(self) -> &'static str {
        match self {
            CarrierPlatform::PatchSm => "LobModule:DAISY_PATCH_SM",
            CarrierPlatform::Seed2Dfm => "LobModule:DAISY_SEED2_DFM",
        }
    }

    fn profile_name(self) -> &'static str {
        match self {
            CarrierPlatform::PatchSm => "DAISY_PATCH_SM",
            CarrierPlatform::Seed2Dfm => "DAISY_SEED2_DFM",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioChannel {
    Left,
    Right,
}

impl AudioChannel {
    fn in_pin(self) -> &'static str {
        match self {
            AudioChannel::Left => "AUDIO_IN_L",
            AudioChannel::Right => "AUDIO_IN_R",
        }
    }

    fn out_pin(self) -> &'static str {
        match self {
            AudioChannel::Left => "AUDIO_OUT_L",
            AudioChannel::Right => "AUDIO_OUT_R",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarrierError {
    UnknownProfile(&'static str),
    UnknownSomPin {
        platform: &'static str,
        pin: String,
    },
    ChannelOutOfRange {
        kind: &'static str,
        channel: u8,
    },
    UnsupportedPlatform {
        platform: &'static str,
        feature: &'static str,
    },
}

impl fmt::Display for CarrierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CarrierError::UnknownProfile(name) => write!(f, "unknown carrier profile {name}"),
            CarrierError::UnknownSomPin { platform, pin } => {
                write!(f, "{platform} has no SOM pin named {pin}")
            }
            CarrierError::ChannelOutOfRange { kind, channel } => {
                write!(f, "{kind} channel {channel} is out of range")
            }
            CarrierError::UnsupportedPlatform { platform, feature } => {
                write!(f, "{feature} is not available for {platform}")
            }
        }
    }
}

impl std::error::Error for CarrierError {}

/// Export a deterministic firmware/Oopsy-facing pinmap for carrier circuits.
///
/// The artifact names the SOM profile and every net bound to a SOM pin. It does
/// not try to infer firmware semantics beyond the circuit's own net names; those
/// names are the stable bridge from carrier hardware to generated board defs.
pub fn firmware_pinmap_json(circuit: &dyn CircuitSource) -> String {
    let parts: std::collections::HashMap<&str, &Part> = circuit
        .parts()
        .iter()
        .map(|p| (p.refdes.0.as_str(), p))
        .collect();
    let mut modules = circuit
        .parts()
        .iter()
        .filter_map(|part| {
            let footprint = part.footprint.as_deref()?;
            let (lib, name) = footprint.split_once(':')?;
            (lib == crate::subboard::SUBBOARD_LIB)
                .then(|| crate::subboard::profile(name).map(|profile| (part, footprint, profile)))
                .flatten()
        })
        .map(|(part, footprint, profile)| {
            let mut bindings = Vec::new();
            for net in circuit.nets() {
                for pin in net.pins.iter().filter(|pin| pin.refdes == part.refdes) {
                    let Some(profile_pin) = profile.pin_for(&pin.pin) else {
                        continue;
                    };
                    let mut panel_refs: Vec<String> = net
                        .pins
                        .iter()
                        .filter(|other| other.refdes != part.refdes)
                        .filter_map(|other| {
                            let panel_part = parts.get(other.refdes.0.as_str())?;
                            is_panel_firmware_binding(panel_part)
                                .then(|| format!("{}:{}", other.refdes, other.pin))
                        })
                        .collect();
                    panel_refs.sort();
                    let mut expander_refs: Vec<String> = net
                        .pins
                        .iter()
                        .filter(|other| other.refdes != part.refdes)
                        .filter_map(|other| {
                            let expander = parts.get(other.refdes.0.as_str())?;
                            is_expander_binding(expander)
                                .then(|| format!("{}:{}", other.refdes, other.pin))
                        })
                        .collect();
                    expander_refs.sort();
                    bindings.push(json!({
                        "net": net.name,
                        "requested_pin": pin.pin,
                        "pin": profile_pin.name,
                        "pad": profile_pin.pad,
                        "capabilities": profile_pin.capabilities.iter().map(|c| format!("{c:?}")).collect::<Vec<_>>(),
                        "panel_refs": panel_refs,
                        "expander_refs": expander_refs,
                    }));
                }
            }
            bindings.sort_by(|a, b| {
                let ak = (
                    a["net"].as_str().unwrap_or_default(),
                    a["pin"].as_str().unwrap_or_default(),
                    a["requested_pin"].as_str().unwrap_or_default(),
                );
                let bk = (
                    b["net"].as_str().unwrap_or_default(),
                    b["pin"].as_str().unwrap_or_default(),
                    b["requested_pin"].as_str().unwrap_or_default(),
                );
                ak.cmp(&bk)
            });
            json!({
                "refdes": part.refdes.0,
                "profile": profile.name,
                "profile_id": profile.footprint,
                "footprint": footprint,
                "eurorack_conditioned": profile.eurorack_conditioned,
                "bindings": bindings,
            })
        })
        .collect::<Vec<_>>();
    modules.sort_by(|a, b| {
        a["refdes"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["refdes"].as_str().unwrap_or_default())
    });
    let mut expanders = circuit
        .parts()
        .iter()
        .filter(|part| is_expander_binding(part))
        .map(|part| {
            let mut pins = Vec::new();
            for net in circuit.nets() {
                for pin in net.pins.iter().filter(|pin| pin.refdes == part.refdes) {
                    pins.push(json!({
                        "pin": pin.pin,
                        "net": net.name,
                    }));
                }
            }
            pins.sort_by(|a, b| {
                natural_pin_key(a["pin"].as_str().unwrap_or_default())
                    .cmp(&natural_pin_key(b["pin"].as_str().unwrap_or_default()))
            });
            json!({
                "refdes": part.refdes.0,
                "footprint": part.footprint,
                "pins": pins,
            })
        })
        .collect::<Vec<_>>();
    expanders.sort_by(|a, b| {
        a["refdes"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["refdes"].as_str().unwrap_or_default())
    });

    serde_json::to_string_pretty(&json!({
        "schema": "legion-of-bom.carrier-pinmap.v1",
        "circuit": circuit.name(),
        "modules": modules,
        "expanders": expanders,
    }))
    .expect("serializing carrier pinmap cannot fail")
}

fn is_panel_firmware_binding(part: &Part) -> bool {
    let fp = part.footprint.as_deref().unwrap_or_default();
    let value = part.value.as_str();
    contains_any_ci(
        fp,
        &[
            "Connector_Audio",
            "Jack_3.5",
            "PJ398",
            "Thonkiconn",
            "Potentiometer",
            "SW_",
            "Switch",
            "TestPoint",
        ],
    ) || contains_any_ci(
        value,
        &[
            "jack",
            "audio in",
            "audio out",
            "cv in",
            "cv out",
            "gate",
            "trigger",
            "test point",
        ],
    )
}

fn contains_any_ci(haystack: &str, needles: &[&str]) -> bool {
    let haystack = haystack.to_ascii_lowercase();
    needles
        .iter()
        .any(|needle| haystack.contains(&needle.to_ascii_lowercase()))
}

fn is_expander_binding(part: &Part) -> bool {
    let value = part.value.as_str();
    let fp = part.footprint.as_deref().unwrap_or_default();
    contains_any_ci(value, &["expander"])
        || (contains_any_ci(value, &["bus"])
            && contains_any_ci(fp, &["PinHeader", "IDC", "Connector"]))
}

fn natural_pin_key(pin: &str) -> (u8, u32, &str) {
    match pin.parse::<u32>() {
        Ok(n) => (0, n, pin),
        Err(_) => (1, 0, pin),
    }
}

/// Builder for a carrier board around one SOM/sub-board.
#[derive(Debug, Clone)]
pub struct CarrierBuilder {
    platform: CarrierPlatform,
    module_refdes: RefDes,
    circuit: Circuit,
    ground_net: String,
}

impl CarrierBuilder {
    pub fn new(
        name: impl Into<String>,
        platform: CarrierPlatform,
        module_refdes: impl Into<RefDes>,
    ) -> Result<Self, CarrierError> {
        if crate::subboard::profile(platform.profile_name()).is_none() {
            return Err(CarrierError::UnknownProfile(platform.profile_name()));
        }
        let module_refdes = module_refdes.into();
        let circuit = Circuit {
            name: name.into(),
            parts: vec![Part::new(module_refdes.clone(), platform.module_value())
                .with_footprint(platform.module_footprint())],
            nets: Vec::new(),
        };
        Ok(Self {
            platform,
            module_refdes,
            circuit,
            ground_net: "GND".into(),
        })
    }

    pub fn patch_sm(
        name: impl Into<String>,
        module_refdes: impl Into<RefDes>,
    ) -> Result<Self, CarrierError> {
        Self::new(name, CarrierPlatform::PatchSm, module_refdes)
    }

    pub fn seed2_dfm(
        name: impl Into<String>,
        module_refdes: impl Into<RefDes>,
    ) -> Result<Self, CarrierError> {
        Self::new(name, CarrierPlatform::Seed2Dfm, module_refdes)
    }

    pub fn circuit(&self) -> &Circuit {
        &self.circuit
    }

    pub fn into_circuit(self) -> Circuit {
        self.circuit
    }

    pub fn add_part(&mut self, part: Part) -> &mut Self {
        self.circuit.parts.push(part);
        self
    }

    pub fn bind_to_som(
        &mut self,
        net: impl Into<String>,
        local: PinRef,
        som_pin: impl Into<String>,
    ) -> Result<&mut Self, CarrierError> {
        let som_pin = som_pin.into();
        self.require_som_pin(&som_pin)?;
        self.connect(
            net,
            vec![local, PinRef::new(self.module_refdes.clone(), som_pin)],
        );
        Ok(self)
    }

    pub fn bind_net_to_som(
        &mut self,
        net: impl Into<String>,
        som_pin: impl Into<String>,
    ) -> Result<&mut Self, CarrierError> {
        let som_pin = som_pin.into();
        self.require_som_pin(&som_pin)?;
        self.connect(net, vec![PinRef::new(self.module_refdes.clone(), som_pin)]);
        Ok(self)
    }

    pub fn expander_header<I, P, N>(&mut self, refdes: impl Into<RefDes>, signals: I) -> &mut Self
    where
        I: IntoIterator<Item = (P, N)>,
        P: Into<String>,
        N: Into<String>,
    {
        let refdes = refdes.into();
        self.circuit.parts.push(
            Part::new(refdes.clone(), "expander header")
                .with_footprint(DEFAULT_EXPANDER_HEADER_FOOTPRINT),
        );
        for (pin, net) in signals {
            self.connect(net.into(), vec![PinRef::new(refdes.clone(), pin.into())]);
        }
        self
    }

    pub fn audio_input(
        &mut self,
        jack_refdes: impl Into<RefDes>,
        channel: AudioChannel,
    ) -> Result<&mut Self, CarrierError> {
        let refdes = jack_refdes.into();
        self.add_jack(refdes.clone(), "audio in");
        self.bind_to_som(
            channel.in_pin(),
            PinRef::new(refdes.clone(), "T"),
            channel.in_pin(),
        )?;
        self.connect_ground(PinRef::new(refdes, "S"));
        Ok(self)
    }

    pub fn audio_output(
        &mut self,
        jack_refdes: impl Into<RefDes>,
        channel: AudioChannel,
    ) -> Result<&mut Self, CarrierError> {
        let refdes = jack_refdes.into();
        self.add_jack(refdes.clone(), "audio out");
        self.bind_to_som(
            channel.out_pin(),
            PinRef::new(refdes.clone(), "T"),
            channel.out_pin(),
        )?;
        self.connect_ground(PinRef::new(refdes, "S"));
        Ok(self)
    }

    pub fn cv_input(
        &mut self,
        jack_refdes: impl Into<RefDes>,
        channel: u8,
    ) -> Result<&mut Self, CarrierError> {
        let som_pin = cv_in_pin(channel)?;
        let refdes = jack_refdes.into();
        self.add_jack(refdes.clone(), "cv in");
        self.bind_to_som(som_pin, PinRef::new(refdes.clone(), "T"), som_pin)?;
        self.connect_ground(PinRef::new(refdes, "S"));
        Ok(self)
    }

    pub fn cv_output(
        &mut self,
        jack_refdes: impl Into<RefDes>,
        channel: u8,
    ) -> Result<&mut Self, CarrierError> {
        let som_pin = cv_out_pin(channel)?;
        let refdes = jack_refdes.into();
        self.add_jack(refdes.clone(), "cv out");
        self.bind_to_som(som_pin, PinRef::new(refdes.clone(), "T"), som_pin)?;
        self.connect_ground(PinRef::new(refdes, "S"));
        Ok(self)
    }

    pub fn gate_input(
        &mut self,
        jack_refdes: impl Into<RefDes>,
        channel: u8,
    ) -> Result<&mut Self, CarrierError> {
        let som_pin = gate_in_pin(channel)?;
        let refdes = jack_refdes.into();
        self.add_jack(refdes.clone(), "gate in");
        self.bind_to_som(som_pin, PinRef::new(refdes.clone(), "T"), som_pin)?;
        self.connect_ground(PinRef::new(refdes, "S"));
        Ok(self)
    }

    pub fn gate_output(
        &mut self,
        jack_refdes: impl Into<RefDes>,
        channel: u8,
    ) -> Result<&mut Self, CarrierError> {
        let som_pin = gate_out_pin(channel)?;
        let refdes = jack_refdes.into();
        self.add_jack(refdes.clone(), "gate out");
        self.bind_to_som(som_pin, PinRef::new(refdes.clone(), "T"), som_pin)?;
        self.connect_ground(PinRef::new(refdes, "S"));
        Ok(self)
    }

    pub fn control_pot(
        &mut self,
        pot_refdes: impl Into<RefDes>,
        cv_channel: u8,
        top_net: impl Into<String>,
        bottom_net: impl Into<String>,
    ) -> Result<&mut Self, CarrierError> {
        let som_pin = cv_in_pin(cv_channel)?;
        let refdes = pot_refdes.into();
        self.circuit
            .parts
            .push(Part::new(refdes.clone(), "100k").with_footprint(DEFAULT_POT_FOOTPRINT));
        self.connect(top_net, vec![PinRef::new(refdes.clone(), "1")]);
        self.bind_to_som(som_pin, PinRef::new(refdes.clone(), "2"), som_pin)?;
        self.connect(bottom_net, vec![PinRef::new(refdes, "3")]);
        Ok(self)
    }

    /// Seed2 DFM Eurorack CV input: panel jack -> divider/series impedance ->
    /// clamped, filtered ADC node -> raw STM32 ADC pin.
    pub fn seed2_cv_input_front_end(
        &mut self,
        block: impl AsRef<str>,
        jack_refdes: impl Into<RefDes>,
        som_pin: impl Into<String>,
    ) -> Result<&mut Self, CarrierError> {
        self.require_seed2("Seed2 CV input front-end")?;
        let block = block.as_ref();
        let som_pin = som_pin.into();
        self.require_som_pin(&som_pin)?;

        let jack = jack_refdes.into();
        let r_series = prefixed("R", block, "A");
        let r_div = prefixed("R", block, "B");
        let c_filter = prefixed("C", block, "A");
        let d_hi = prefixed("D", block, "H");
        let d_lo = prefixed("D", block, "L");
        let panel_net = format!("{block}_CV_PANEL");
        let adc_net = format!("{block}_CV_ADC");

        self.add_jack(jack.clone(), "cv in");
        self.add_resistor(r_series.clone(), "100k");
        self.add_resistor(r_div.clone(), "33k");
        self.add_capacitor(c_filter.clone(), "1n");
        self.add_diode(d_hi.clone(), "BAT54");
        self.add_diode(d_lo.clone(), "BAT54");

        self.connect(
            panel_net,
            vec![
                PinRef::new(jack.clone(), "T"),
                PinRef::new(r_series.clone(), "1"),
            ],
        );
        self.connect_ground(PinRef::new(jack, "S"));
        self.bind_to_som(adc_net.clone(), PinRef::new(r_series, "2"), som_pin)?;
        self.connect(
            adc_net,
            vec![
                PinRef::new(r_div.clone(), "1"),
                PinRef::new(c_filter.clone(), "1"),
                PinRef::new(d_hi.clone(), "A"),
                PinRef::new(d_lo.clone(), "K"),
            ],
        );
        self.connect_ground(PinRef::new(r_div, "2"));
        self.connect_ground(PinRef::new(c_filter, "2"));
        self.connect("3V3", vec![PinRef::new(d_hi, "K")]);
        self.connect_ground(PinRef::new(d_lo, "A"));
        Ok(self)
    }

    /// Seed2 DFM audio input: AC-couple the jack, add input impedance, then bias
    /// the codec input at AUDIO_VCOM.
    pub fn seed2_audio_input_front_end(
        &mut self,
        block: impl AsRef<str>,
        jack_refdes: impl Into<RefDes>,
        channel: AudioChannel,
    ) -> Result<&mut Self, CarrierError> {
        self.require_seed2("Seed2 audio input front-end")?;
        let block = block.as_ref();
        let jack = jack_refdes.into();
        let c_in = prefixed("C", block, "A");
        let r_in = prefixed("R", block, "A");
        let r_bias = prefixed("R", block, "B");
        let panel_net = format!("{block}_AUDIO_IN_PANEL");
        let ac_net = format!("{block}_AUDIO_IN_AC");
        let codec_net = format!("{block}_AUDIO_IN_CODEC");

        self.add_jack(jack.clone(), "audio in");
        self.add_capacitor(c_in.clone(), "1u");
        self.add_resistor(r_in.clone(), "1k");
        self.add_resistor(r_bias.clone(), "100k");

        self.connect(
            panel_net,
            vec![
                PinRef::new(jack.clone(), "T"),
                PinRef::new(c_in.clone(), "1"),
            ],
        );
        self.connect_ground(PinRef::new(jack, "S"));
        self.connect(
            ac_net,
            vec![PinRef::new(c_in, "2"), PinRef::new(r_in.clone(), "1")],
        );
        self.bind_to_som(codec_net.clone(), PinRef::new(r_in, "2"), channel.in_pin())?;
        self.connect(codec_net, vec![PinRef::new(r_bias.clone(), "1")]);
        self.bind_to_som("AUDIO_VCOM", PinRef::new(r_bias, "2"), "AUDIO_VCOM")?;
        Ok(self)
    }

    /// Seed2 DFM audio output: raw codec output -> series resistor -> AC coupling
    /// cap -> output jack with a bleed resistor.
    pub fn seed2_audio_output_front_end(
        &mut self,
        block: impl AsRef<str>,
        jack_refdes: impl Into<RefDes>,
        channel: AudioChannel,
    ) -> Result<&mut Self, CarrierError> {
        self.require_seed2("Seed2 audio output front-end")?;
        let block = block.as_ref();
        let jack = jack_refdes.into();
        let r_out = prefixed("R", block, "A");
        let c_out = prefixed("C", block, "A");
        let r_bleed = prefixed("R", block, "B");
        let codec_net = format!("{block}_AUDIO_OUT_CODEC");
        let ac_net = format!("{block}_AUDIO_OUT_AC");
        let panel_net = format!("{block}_AUDIO_OUT_PANEL");

        self.add_jack(jack.clone(), "audio out");
        self.add_resistor(r_out.clone(), "100R");
        self.add_capacitor(c_out.clone(), "10u");
        self.add_resistor(r_bleed.clone(), "100k");

        self.bind_to_som(
            codec_net,
            PinRef::new(r_out.clone(), "1"),
            seed2_audio_out_pos(channel),
        )?;
        self.connect(
            ac_net,
            vec![PinRef::new(r_out, "2"), PinRef::new(c_out.clone(), "1")],
        );
        self.connect(
            panel_net,
            vec![
                PinRef::new(c_out, "2"),
                PinRef::new(jack.clone(), "T"),
                PinRef::new(r_bleed.clone(), "1"),
            ],
        );
        self.connect_ground(PinRef::new(r_bleed, "2"));
        self.connect_ground(PinRef::new(jack, "S"));
        Ok(self)
    }

    /// Seed2 DFM gate/trigger input: panel jack -> series impedance -> clamped,
    /// pulled-down GPIO node.
    pub fn seed2_gate_input_front_end(
        &mut self,
        block: impl AsRef<str>,
        jack_refdes: impl Into<RefDes>,
        som_pin: impl Into<String>,
    ) -> Result<&mut Self, CarrierError> {
        self.require_seed2("Seed2 gate input front-end")?;
        let block = block.as_ref();
        let som_pin = som_pin.into();
        self.require_som_pin(&som_pin)?;
        let jack = jack_refdes.into();
        let r_series = prefixed("R", block, "A");
        let r_pull = prefixed("R", block, "B");
        let d_hi = prefixed("D", block, "H");
        let d_lo = prefixed("D", block, "L");
        let panel_net = format!("{block}_GATE_PANEL");
        let gpio_net = format!("{block}_GATE_GPIO");

        self.add_jack(jack.clone(), "gate in");
        self.add_resistor(r_series.clone(), "100k");
        self.add_resistor(r_pull.clone(), "100k");
        self.add_diode(d_hi.clone(), "BAT54");
        self.add_diode(d_lo.clone(), "BAT54");

        self.connect(
            panel_net,
            vec![
                PinRef::new(jack.clone(), "T"),
                PinRef::new(r_series.clone(), "1"),
            ],
        );
        self.connect_ground(PinRef::new(jack, "S"));
        self.bind_to_som(gpio_net.clone(), PinRef::new(r_series, "2"), som_pin)?;
        self.connect(
            gpio_net,
            vec![
                PinRef::new(r_pull.clone(), "1"),
                PinRef::new(d_hi.clone(), "A"),
                PinRef::new(d_lo.clone(), "K"),
            ],
        );
        self.connect_ground(PinRef::new(r_pull, "2"));
        self.connect("3V3", vec![PinRef::new(d_hi, "K")]);
        self.connect_ground(PinRef::new(d_lo, "A"));
        Ok(self)
    }

    /// Seed2 DFM logic/gate output: GPIO pin -> output resistor -> panel jack.
    pub fn seed2_gate_output_front_end(
        &mut self,
        block: impl AsRef<str>,
        jack_refdes: impl Into<RefDes>,
        som_pin: impl Into<String>,
    ) -> Result<&mut Self, CarrierError> {
        self.require_seed2("Seed2 gate output front-end")?;
        let block = block.as_ref();
        let som_pin = som_pin.into();
        self.require_som_pin(&som_pin)?;
        let jack = jack_refdes.into();
        let r_out = prefixed("R", block, "A");
        let r_pull = prefixed("R", block, "B");
        let gpio_net = format!("{block}_GATE_GPIO");
        let panel_net = format!("{block}_GATE_PANEL");

        self.add_jack(jack.clone(), "gate out");
        self.add_resistor(r_out.clone(), "1k");
        self.add_resistor(r_pull.clone(), "100k");
        self.bind_to_som(gpio_net, PinRef::new(r_out.clone(), "1"), som_pin)?;
        self.connect(
            panel_net,
            vec![
                PinRef::new(r_out, "2"),
                PinRef::new(jack.clone(), "T"),
                PinRef::new(r_pull.clone(), "1"),
            ],
        );
        self.connect_ground(PinRef::new(r_pull, "2"));
        self.connect_ground(PinRef::new(jack, "S"));
        Ok(self)
    }

    /// Seed2 DFM CV output: DAC pin -> op-amp buffer/scale block -> panel jack.
    pub fn seed2_cv_output_front_end(
        &mut self,
        block: impl AsRef<str>,
        jack_refdes: impl Into<RefDes>,
        som_pin: impl Into<String>,
    ) -> Result<&mut Self, CarrierError> {
        self.require_seed2("Seed2 CV output front-end")?;
        let block = block.as_ref();
        let som_pin = som_pin.into();
        self.require_som_pin(&som_pin)?;
        let jack = jack_refdes.into();
        let opamp = prefixed("U", block, "A");
        let r_in = prefixed("R", block, "A");
        let r_fb = prefixed("R", block, "B");
        let r_out = prefixed("R", block, "C");
        let dac_net = format!("{block}_DAC_RAW");
        let op_in_net = format!("{block}_CV_BUF_IN");
        let op_out_net = format!("{block}_CV_BUF_OUT");
        let panel_net = format!("{block}_CV_PANEL");

        self.add_jack(jack.clone(), "cv out");
        self.add_resistor(r_in.clone(), "10k");
        self.add_resistor(r_fb.clone(), "20k");
        self.add_resistor(r_out.clone(), "1k");
        self.circuit.parts.push(
            Part::new(opamp.clone(), "rail-to-rail op amp").with_footprint(DEFAULT_OPAMP_FOOTPRINT),
        );

        self.bind_to_som(dac_net, PinRef::new(r_in.clone(), "1"), som_pin)?;
        self.connect(
            op_in_net.clone(),
            vec![PinRef::new(r_in, "2"), PinRef::new(opamp.clone(), "3")],
        );
        self.connect_ground(PinRef::new(opamp.clone(), "2"));
        self.connect(
            op_out_net,
            vec![
                PinRef::new(opamp.clone(), "1"),
                PinRef::new(r_fb.clone(), "1"),
                PinRef::new(r_out.clone(), "1"),
            ],
        );
        self.connect(op_in_net, vec![PinRef::new(r_fb, "2")]);
        self.connect(
            panel_net,
            vec![PinRef::new(r_out, "2"), PinRef::new(jack.clone(), "T")],
        );
        self.connect("12V", vec![PinRef::new(opamp.clone(), "8")]);
        self.connect("-12V", vec![PinRef::new(opamp, "4")]);
        self.connect_ground(PinRef::new(jack, "S"));
        Ok(self)
    }

    /// Add local decoupling from Seed2 DFM power pins to the carrier ground net.
    pub fn seed2_power_hygiene(
        &mut self,
        block: impl AsRef<str>,
    ) -> Result<&mut Self, CarrierError> {
        self.require_seed2("Seed2 power hygiene")?;
        let block = block.as_ref();
        for (suffix, value, net, som_pin) in [
            ("V", "10u", "VIN", "VIN"),
            ("D", "100n", "3V3", "3V3_D"),
            ("A", "100n", "3V3_A", "3V3_A"),
        ] {
            let cap = prefixed("C", block, suffix);
            self.add_capacitor(cap.clone(), value);
            self.bind_to_som(net, PinRef::new(cap.clone(), "1"), som_pin)?;
            self.connect_ground(PinRef::new(cap, "2"));
        }
        Ok(self)
    }

    /// Add a named one-pin test point to any carrier net for calibration or bring-up.
    pub fn test_point(&mut self, refdes: impl Into<RefDes>, net: impl Into<String>) -> &mut Self {
        let refdes = refdes.into();
        self.circuit.parts.push(
            Part::new(refdes.clone(), "test point").with_footprint(DEFAULT_TESTPOINT_FOOTPRINT),
        );
        self.connect(net, vec![PinRef::new(refdes, "1")]);
        self
    }

    fn add_jack(&mut self, refdes: RefDes, value: &str) {
        self.circuit
            .parts
            .push(Part::new(refdes, value).with_footprint(DEFAULT_JACK_FOOTPRINT));
    }

    fn add_resistor(&mut self, refdes: RefDes, value: &str) {
        self.circuit
            .parts
            .push(Part::new(refdes, value).with_footprint(DEFAULT_RESISTOR_FOOTPRINT));
    }

    fn add_capacitor(&mut self, refdes: RefDes, value: &str) {
        self.circuit
            .parts
            .push(Part::new(refdes, value).with_footprint(DEFAULT_CAPACITOR_FOOTPRINT));
    }

    fn add_diode(&mut self, refdes: RefDes, value: &str) {
        self.circuit
            .parts
            .push(Part::new(refdes, value).with_footprint(DEFAULT_DIODE_FOOTPRINT));
    }

    fn connect_ground(&mut self, pin: PinRef) {
        let net = self.ground_net.clone();
        self.connect(net, vec![pin]);
    }

    fn connect(&mut self, net: impl Into<String>, pins: Vec<PinRef>) {
        let net = net.into();
        if let Some(existing) = self.circuit.nets.iter_mut().find(|n| n.name == net) {
            existing.pins.extend(pins);
        } else {
            self.circuit.nets.push(Net::new(net, pins));
        }
    }

    fn require_som_pin(&self, pin: &str) -> Result<(), CarrierError> {
        let profile = crate::subboard::profile(self.platform.profile_name())
            .ok_or(CarrierError::UnknownProfile(self.platform.profile_name()))?;
        profile
            .pad_for(pin)
            .map(|_| ())
            .ok_or_else(|| CarrierError::UnknownSomPin {
                platform: self.platform.profile_name(),
                pin: pin.to_string(),
            })
    }

    fn require_seed2(&self, feature: &'static str) -> Result<(), CarrierError> {
        if self.platform == CarrierPlatform::Seed2Dfm {
            Ok(())
        } else {
            Err(CarrierError::UnsupportedPlatform {
                platform: self.platform.profile_name(),
                feature,
            })
        }
    }
}

fn prefixed(kind: &str, block: &str, suffix: &str) -> RefDes {
    RefDes(format!("{kind}{block}{suffix}"))
}

fn seed2_audio_out_pos(channel: AudioChannel) -> &'static str {
    match channel {
        AudioChannel::Left => "AUDIO_OUT_L+",
        AudioChannel::Right => "AUDIO_OUT_R+",
    }
}

fn cv_in_pin(channel: u8) -> Result<&'static str, CarrierError> {
    match channel {
        1 => Ok("CV_1"),
        2 => Ok("CV_2"),
        3 => Ok("CV_3"),
        4 => Ok("CV_4"),
        5 => Ok("CV_5"),
        6 => Ok("CV_6"),
        7 => Ok("CV_7"),
        8 => Ok("CV_8"),
        _ => Err(CarrierError::ChannelOutOfRange {
            kind: "CV input",
            channel,
        }),
    }
}

fn cv_out_pin(channel: u8) -> Result<&'static str, CarrierError> {
    match channel {
        1 => Ok("CV_OUT_1"),
        2 => Ok("CV_OUT_2"),
        _ => Err(CarrierError::ChannelOutOfRange {
            kind: "CV output",
            channel,
        }),
    }
}

fn gate_in_pin(channel: u8) -> Result<&'static str, CarrierError> {
    match channel {
        1 => Ok("GATE_IN_1"),
        2 => Ok("GATE_IN_2"),
        _ => Err(CarrierError::ChannelOutOfRange {
            kind: "gate input",
            channel,
        }),
    }
}

fn gate_out_pin(channel: u8) -> Result<&'static str, CarrierError> {
    match channel {
        1 => Ok("GATE_OUT_1"),
        2 => Ok("GATE_OUT_2"),
        _ => Err(CarrierError::ChannelOutOfRange {
            kind: "gate output",
            channel,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::CircuitSource;
    use crate::validate_carrier;

    fn has_pin(circuit: &Circuit, net: &str, refdes: &str, pin: &str) -> bool {
        circuit
            .nets
            .iter()
            .any(|n| n.name == net && n.pins.iter().any(|p| p.refdes.0 == refdes && p.pin == pin))
    }

    #[test]
    fn patch_sm_carrier_binds_panel_hardware_to_semantic_som_pins() {
        let mut carrier = CarrierBuilder::patch_sm("patch-init-ish", "M1").unwrap();
        carrier
            .audio_input("J1", AudioChannel::Left)
            .unwrap()
            .audio_output("J2", AudioChannel::Right)
            .unwrap()
            .cv_input("J3", 1)
            .unwrap()
            .control_pot("RV1", 5, "3V3", "GND")
            .unwrap()
            .gate_input("J4", 1)
            .unwrap()
            .gate_output("J5", 2)
            .unwrap();

        let circuit = carrier.into_circuit();
        assert_eq!(circuit.parts().len(), 7);
        assert!(circuit.parts().iter().any(|p| {
            p.refdes.0 == "M1" && p.footprint.as_deref() == Some("LobModule:DAISY_PATCH_SM")
        }));

        assert!(has_pin(&circuit, "AUDIO_IN_L", "J1", "T"));
        assert!(has_pin(&circuit, "AUDIO_IN_L", "M1", "AUDIO_IN_L"));
        assert!(has_pin(&circuit, "AUDIO_OUT_R", "J2", "T"));
        assert!(has_pin(&circuit, "AUDIO_OUT_R", "M1", "AUDIO_OUT_R"));
        assert!(has_pin(&circuit, "CV_1", "J3", "T"));
        assert!(has_pin(&circuit, "CV_1", "M1", "CV_1"));
        assert!(has_pin(&circuit, "CV_5", "RV1", "2"));
        assert!(has_pin(&circuit, "CV_5", "M1", "CV_5"));
        assert!(has_pin(&circuit, "GATE_IN_1", "M1", "GATE_IN_1"));
        assert!(has_pin(&circuit, "GATE_OUT_2", "M1", "GATE_OUT_2"));
        assert!(has_pin(&circuit, "GND", "J1", "S"));
        assert!(has_pin(&circuit, "GND", "RV1", "3"));
    }

    #[test]
    fn carrier_rejects_unknown_som_pin_names() {
        let mut carrier = CarrierBuilder::patch_sm("bad", "M1").unwrap();
        carrier.add_part(Part::new("J1", "in").with_footprint(DEFAULT_JACK_FOOTPRINT));

        let err = carrier
            .bind_to_som("BAD", PinRef::new("J1", "T"), "NO_SUCH_PIN")
            .unwrap_err();
        assert_eq!(
            err,
            CarrierError::UnknownSomPin {
                platform: "DAISY_PATCH_SM",
                pin: "NO_SUCH_PIN".into()
            }
        );
    }

    #[test]
    fn seed2_dfm_carrier_can_bind_raw_som_pins() {
        let mut carrier = CarrierBuilder::seed2_dfm("seed2-carrier", "M1").unwrap();
        carrier
            .add_part(Part::new("RV1", "10k").with_footprint(DEFAULT_POT_FOOTPRINT))
            .bind_to_som("POT_1", PinRef::new("RV1", "2"), "A1")
            .unwrap()
            .add_part(Part::new("J1", "audio out").with_footprint(DEFAULT_JACK_FOOTPRINT))
            .bind_to_som("OUT_L_P", PinRef::new("J1", "T"), "AUDIO_OUT_L+")
            .unwrap();

        let circuit = carrier.into_circuit();
        assert!(circuit.parts().iter().any(|p| {
            p.refdes.0 == "M1" && p.footprint.as_deref() == Some("LobModule:DAISY_SEED2_DFM")
        }));
        assert!(has_pin(&circuit, "POT_1", "M1", "A1"));
        assert!(has_pin(&circuit, "OUT_L_P", "M1", "AUDIO_OUT_L+"));
    }

    #[test]
    fn seed2_front_end_blocks_break_raw_panel_to_som_nets() {
        let mut carrier = CarrierBuilder::seed2_dfm("seed2-front-ends", "M1").unwrap();
        carrier
            .seed2_cv_input_front_end("CV1", "J1", "A1")
            .unwrap()
            .seed2_audio_input_front_end("AI1", "J2", AudioChannel::Left)
            .unwrap()
            .seed2_audio_output_front_end("AO1", "J3", AudioChannel::Right)
            .unwrap()
            .seed2_gate_input_front_end("GI1", "J4", "D13")
            .unwrap()
            .seed2_gate_output_front_end("GO1", "J5", "D14")
            .unwrap()
            .seed2_cv_output_front_end("CO1", "J6", "DAC1")
            .unwrap()
            .seed2_power_hygiene("PWR")
            .unwrap()
            .test_point("TP1", "CV1_CV_ADC");

        let circuit = carrier.into_circuit();
        let outcome = validate_carrier(&circuit);
        assert!(outcome.passed, "{:?}", outcome.findings);
        assert!(has_pin(&circuit, "CV1_CV_ADC", "M1", "A1"));
        assert!(has_pin(&circuit, "AI1_AUDIO_IN_CODEC", "M1", "AUDIO_IN_L"));
        assert!(has_pin(
            &circuit,
            "AO1_AUDIO_OUT_CODEC",
            "M1",
            "AUDIO_OUT_R+"
        ));
        assert!(has_pin(&circuit, "GI1_GATE_GPIO", "M1", "D13"));
        assert!(has_pin(&circuit, "GO1_GATE_GPIO", "M1", "D14"));
        assert!(has_pin(&circuit, "CO1_DAC_RAW", "M1", "DAC1"));
        assert!(has_pin(&circuit, "3V3_A", "M1", "3V3_A"));
        assert!(has_pin(&circuit, "CV1_CV_ADC", "TP1", "1"));
    }

    #[test]
    fn seed2_front_end_methods_reject_patch_sm() {
        let mut carrier = CarrierBuilder::patch_sm("patch", "M1").unwrap();
        assert_eq!(
            carrier
                .seed2_cv_input_front_end("CV1", "J1", "A1")
                .unwrap_err(),
            CarrierError::UnsupportedPlatform {
                platform: "DAISY_PATCH_SM",
                feature: "Seed2 CV input front-end",
            }
        );
    }

    #[test]
    fn firmware_pinmap_exports_patch_sm_panel_bindings_deterministically() {
        let mut carrier = CarrierBuilder::patch_sm("patch-firmware", "M1").unwrap();
        carrier
            .audio_input("J1", AudioChannel::Left)
            .unwrap()
            .cv_input("J2", 1)
            .unwrap()
            .gate_input("J3", 1)
            .unwrap();

        let json = firmware_pinmap_json(&carrier.into_circuit());
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema"], "legion-of-bom.carrier-pinmap.v1");
        assert_eq!(parsed["circuit"], "patch-firmware");
        assert_eq!(parsed["modules"][0]["profile_id"], "DAISY_PATCH_SM");
        assert_eq!(parsed["modules"][0]["eurorack_conditioned"], true);
        let bindings = parsed["modules"][0]["bindings"].as_array().unwrap();
        assert!(bindings.iter().any(|b| {
            b["pin"] == "AUDIO_IN_L"
                && b["panel_refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p == "J1:T")
        }));
        assert_eq!(json, firmware_pinmap_json(&parsed_patch_fixture()));
    }

    #[test]
    fn firmware_pinmap_exports_seed2_front_end_som_bindings() {
        let mut carrier = CarrierBuilder::seed2_dfm("seed2-firmware", "M1").unwrap();
        carrier
            .seed2_cv_input_front_end("CV1", "J1", "A1")
            .unwrap()
            .seed2_audio_output_front_end("AO1", "J2", AudioChannel::Left)
            .unwrap()
            .test_point("TP1", "CV1_CV_ADC");

        let json = firmware_pinmap_json(&carrier.into_circuit());
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["modules"][0]["profile_id"], "DAISY_SEED2_DFM");
        assert_eq!(parsed["modules"][0]["eurorack_conditioned"], false);
        let bindings = parsed["modules"][0]["bindings"].as_array().unwrap();
        assert!(bindings
            .iter()
            .any(|b| b["requested_pin"] == "A1" && b["pin"] == "D16"));
        assert!(bindings
            .iter()
            .any(|b| { b["requested_pin"] == "AUDIO_OUT_L+" && b["pin"] == "AUDIO_OUT_L_POS" }));
        assert!(bindings.iter().any(|b| {
            b["net"] == "CV1_CV_ADC"
                && b["panel_refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p == "TP1:1")
        }));
    }

    #[test]
    fn expander_header_routes_named_bus_and_exports_pinmap() {
        let mut carrier = CarrierBuilder::patch_sm("handpan-voice", "M1").unwrap();
        carrier
            .expander_header(
                "XP1",
                [
                    ("1", "GND"),
                    ("2", "3V3"),
                    ("3", "VOICE_VOCT"),
                    ("4", "VOICE_STRIKE"),
                    ("5", "VOICE_VELOCITY"),
                    ("6", "BRAIN_CLOCK"),
                ],
            )
            .bind_net_to_som("GND", "GND")
            .unwrap()
            .bind_net_to_som("3V3", "3V3")
            .unwrap()
            .bind_net_to_som("VOICE_VOCT", "CV_5")
            .unwrap()
            .bind_net_to_som("VOICE_STRIKE", "GATE_IN_1")
            .unwrap()
            .bind_net_to_som("VOICE_VELOCITY", "CV_6")
            .unwrap()
            .bind_net_to_som("BRAIN_CLOCK", "GATE_IN_2")
            .unwrap();

        let circuit = carrier.into_circuit();
        assert!(has_pin(&circuit, "VOICE_VOCT", "XP1", "3"));
        assert!(has_pin(&circuit, "VOICE_VOCT", "M1", "CV_5"));
        assert!(has_pin(&circuit, "BRAIN_CLOCK", "XP1", "6"));
        assert!(has_pin(&circuit, "BRAIN_CLOCK", "M1", "GATE_IN_2"));

        let json = firmware_pinmap_json(&circuit);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["expanders"][0]["refdes"], "XP1");
        assert_eq!(parsed["expanders"][0]["pins"][2]["net"], "VOICE_VOCT");
        let bindings = parsed["modules"][0]["bindings"].as_array().unwrap();
        assert!(bindings.iter().any(|b| {
            b["net"] == "VOICE_VOCT"
                && b["pin"] == "CV_5"
                && b["expander_refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p == "XP1:3")
        }));
    }

    fn parsed_patch_fixture() -> Circuit {
        let mut carrier = CarrierBuilder::patch_sm("patch-firmware", "M1").unwrap();
        carrier
            .audio_input("J1", AudioChannel::Left)
            .unwrap()
            .cv_input("J2", 1)
            .unwrap()
            .gate_input("J3", 1)
            .unwrap();
        carrier.into_circuit()
    }

    #[test]
    fn carrier_rejects_channels_the_platform_does_not_have() {
        let mut carrier = CarrierBuilder::patch_sm("bad", "M1").unwrap();
        assert_eq!(
            carrier.cv_input("J1", 9).unwrap_err(),
            CarrierError::ChannelOutOfRange {
                kind: "CV input",
                channel: 9
            }
        );
        assert_eq!(
            carrier.gate_output("J2", 3).unwrap_err(),
            CarrierError::ChannelOutOfRange {
                kind: "gate output",
                channel: 3
            }
        );
    }
}
