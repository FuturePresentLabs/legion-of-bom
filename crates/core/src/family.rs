//! The curated circuit families as one type: the unit `lob spec <family>`
//! decides and `lob schematic` renders.
//!
//! A spec file carries its family as an explicit `"family"` tag, so the
//! renderer dispatches on what the spec *says it is* rather than sniffing its
//! shape. Adding a family is one variant plus its arms here — the CLI and
//! PCBBench never learn family names (legion-of-bom-y17.7).
//!
//! Spec files written before the tag existed are still read: an untagged file
//! with `stages` is a fuzz chain, anything else a fuzz pedal — exactly what
//! the CLI's sniffing did, kept only as a reader for old files.

use ooda::{Client, Trace};
use serde::{Deserialize, Serialize};

use crate::mcu_audio::{generate_stm32_codec_spec, render_stm32_codec_skidl, Stm32CodecSpec};
use crate::panel::PanelFile;
use crate::pedal_panel::fuzz_pedal_panel_file;
use crate::spec::{generate_fuzz_pedal_spec, render_skidl, FuzzPedalSpec, SpecError};
use crate::topology::{render_chain_skidl, FuzzChain};

/// A decided spec, of any curated family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "family", rename_all = "kebab-case")]
pub enum Spec {
    FuzzPedal(FuzzPedalSpec),
    /// Written by `lob spec-chain`, which takes constraints `lob spec` does
    /// not; it is not reachable through [`generate`].
    FuzzChain(FuzzChain),
    /// An STM32H7 audio board with one of three codec options.
    Stm32Codec(Stm32CodecSpec),
}

/// The families [`generate`] decides — what `lob spec <family>` accepts.
pub const FAMILIES: &[&str] = &["fuzz-pedal", "stm32-codec"];

/// Unknown family, or a decision that failed.
#[derive(Debug, thiserror::Error)]
pub enum FamilyError {
    #[error("unknown circuit family '{0}' (curated set: {list})", list = FAMILIES.join(", "))]
    Unknown(String),
    #[error(transparent)]
    Spec(#[from] SpecError),
}

/// Decide a spec for `family` from `brief`, recording every decision to
/// `trace`.
pub fn generate(
    family: &str,
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
) -> Result<Spec, FamilyError> {
    match family {
        "fuzz-pedal" => Ok(Spec::FuzzPedal(generate_fuzz_pedal_spec(
            client, trace, brief,
        )?)),
        "stm32-codec" => Ok(Spec::Stm32Codec(generate_stm32_codec_spec(
            client, trace, brief,
        )?)),
        other => Err(FamilyError::Unknown(other.to_string())),
    }
}

impl Spec {
    /// Read a spec file's JSON: by its `family` tag, or — for a file written
    /// before the tag existed — by its shape.
    pub fn from_json(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        if value.get("family").is_some() {
            return serde_json::from_value(value);
        }
        if value.get("stages").is_some() {
            serde_json::from_value(value).map(Spec::FuzzChain)
        } else {
            serde_json::from_value(value).map(Spec::FuzzPedal)
        }
    }

    /// The family tag this spec is written under.
    pub fn family(&self) -> &'static str {
        match self {
            Spec::FuzzPedal(_) => "fuzz-pedal",
            Spec::FuzzChain(_) => "fuzz-chain",
            Spec::Stm32Codec(_) => "stm32-codec",
        }
    }

    /// The SKiDL circuit — a pure function of the spec.
    pub fn render_skidl(&self) -> String {
        match self {
            Spec::FuzzPedal(s) => render_skidl(s),
            Spec::FuzzChain(c) => render_chain_skidl(c),
            Spec::Stm32Codec(s) => render_stm32_codec_skidl(s),
        }
    }

    /// The panel the board is built against, when the family has one — also
    /// a pure function of the spec. `None` means the board has no panel and
    /// its outline is derived from the parts.
    pub fn panel(&self) -> Option<PanelFile> {
        let size = match self {
            Spec::FuzzPedal(s) => s.enclosure_size,
            Spec::FuzzChain(c) => c.enclosure_size,
            // A board, not a front panel: its outline comes from its parts.
            Spec::Stm32Codec(_) => return None,
        };
        Some(fuzz_pedal_panel_file(size, ("RV1", "RV2"), 1.6))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::tests_support::fuzz_pedal_spec;

    #[test]
    fn a_spec_round_trips_under_its_family_tag() {
        let spec = Spec::FuzzPedal(fuzz_pedal_spec());
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["family"], "fuzz-pedal");
        assert_eq!(Spec::from_json(json).unwrap(), spec);
    }

    #[test]
    fn an_untagged_spec_from_before_the_tag_still_reads() {
        let bare = serde_json::to_value(fuzz_pedal_spec()).unwrap();
        assert!(bare.get("family").is_none());
        assert_eq!(
            Spec::from_json(bare).unwrap(),
            Spec::FuzzPedal(fuzz_pedal_spec())
        );
    }

    #[test]
    fn an_mcu_board_spec_round_trips_and_has_no_panel() {
        let spec = Spec::Stm32Codec(Stm32CodecSpec {
            codec: crate::mcu_audio::Codec::Es8388,
        });
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["family"], "stm32-codec");
        assert_eq!(json["codec"], "es8388");
        assert_eq!(Spec::from_json(json).unwrap(), spec);
        assert!(spec.panel().is_none(), "a board has no front panel");
    }

    #[test]
    fn an_unknown_family_fails_loud_and_names_the_curated_set() {
        let client = ooda::ScriptedClient::new(Vec::<String>::new());
        let err = generate("theremin", &client, &mut Trace::new(), "spooky").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("theremin") && msg.contains("fuzz-pedal"),
            "{msg}"
        );
    }
}
