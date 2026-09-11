//! Byte-exact terminal payloads carried by the client protocol.

use crate::contract::AgentId;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::{Engine, alphabet};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const PANE_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

/// Raw terminal bytes encoded with RFC 4648's standard padded base64 alphabet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpaquePaneBytesV1(Vec<u8>);

impl OpaquePaneBytesV1 {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Serialize for OpaquePaneBytesV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&PANE_BASE64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for OpaquePaneBytesV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        PANE_BASE64
            .decode(encoded)
            .map(Self)
            .map_err(|_| D::Error::custom("invalid base64-encoded pane bytes"))
    }
}

/// A byte-exact, fixed-width token proving readiness at a pane replay boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneReadyTokenV1([u8; 32]);

impl PaneReadyTokenV1 {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Serialize for PaneReadyTokenV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&PANE_BASE64.encode(self.0))
    }
}

impl<'de> Deserialize<'de> for PaneReadyTokenV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let bytes = PANE_BASE64
            .decode(encoded)
            .map_err(|_| D::Error::custom("invalid base64-encoded pane-ready token"))?;
        let bytes = <[u8; 32]>::try_from(bytes)
            .map_err(|_| D::Error::custom("pane-ready token must be exactly 32 bytes"))?;
        Ok(Self(bytes))
    }
}

/// The three record kinds in version one of the replayable pane stream.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum PaneFrameKindV1 {
    Output { bytes: OpaquePaneBytesV1 },
    Resize { cols: u16, rows: u16 },
    // A struct variant makes Serde apply `deny_unknown_fields`; internally tagged unit variants
    // otherwise ignore additional map entries.
    End {},
}

/// One strictly validated version-one pane record payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneFrameV1 {
    #[serde(deserialize_with = "deserialize_pane_frame_version")]
    version: u8,
    pub agent_id: AgentId,
    pub seq: u64,
    pub frame: PaneFrameKindV1,
}

impl PaneFrameV1 {
    pub fn new(agent_id: AgentId, seq: u64, frame: PaneFrameKindV1) -> Self {
        Self {
            version: 1,
            agent_id,
            seq,
            frame,
        }
    }
}

fn deserialize_pane_frame_version<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u8::deserialize(deserializer)?;
    if version == 1 {
        Ok(version)
    } else {
        Err(D::Error::custom("unsupported pane frame version"))
    }
}
