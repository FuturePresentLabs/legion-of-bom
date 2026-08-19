//! Carrier-board helpers for bought DSP modules / SOMs.
//!
//! A carrier is still just a [`Circuit`](crate::model::Circuit): one part is the
//! sub-board (`LobModule:DAISY_PATCH_SM` / `LobModule:DAISY_SEED2_DFM`), panel hardware is
//! ordinary parts, and nets connect them explicitly. This module only gives that
//! pattern names so a carrier can say "audio input jack to AUDIO_IN_L" instead
//! of hand-spelling the same netlist every time.

use std::fmt;

use crate::model::{Circuit, Net, Part, PinRef, RefDes};

pub const DEFAULT_JACK_FOOTPRINT: &str = "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical";
pub const DEFAULT_POT_FOOTPRINT: &str =
    "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical";

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
    UnknownSomPin { platform: &'static str, pin: String },
    ChannelOutOfRange { kind: &'static str, channel: u8 },
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
        }
    }
}

impl std::error::Error for CarrierError {}

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

    fn add_jack(&mut self, refdes: RefDes, value: &str) {
        self.circuit
            .parts
            .push(Part::new(refdes, value).with_footprint(DEFAULT_JACK_FOOTPRINT));
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
