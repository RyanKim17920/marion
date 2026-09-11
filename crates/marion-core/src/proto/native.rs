//! Exact native process-launch values carried by the client↔supervisor protocol.

use std::ffi::{OsStr, OsString};
use std::fmt;

use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::{Engine, alphabet};
use serde::de::{Error as _, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const OPAQUE_OS_VALUE_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

/// An operating-system value encoded with RFC 4648's standard padded base64 alphabet.
///
/// JSON strings cannot represent every Unix path, argument, or environment value. This type
/// carries their bytes without attempting a UTF-8 conversion; the documented alphabet and
/// required padding keep the JSON representation stable across peers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpaqueOsValueV1(Vec<u8>);

impl OpaqueOsValueV1 {
    /// Copy an operating-system string without a text conversion.
    pub fn from_os_str(value: &OsStr) -> Result<Self, NativeOsValueConversionError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            Ok(Self(value.as_bytes().to_vec()))
        }

        #[cfg(not(unix))]
        {
            let _ = value;
            Err(NativeOsValueConversionError::UnsupportedPlatform)
        }
    }

    /// Reconstruct an operating-system string without a text conversion.
    pub fn to_os_string(&self) -> Result<OsString, NativeOsValueConversionError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            Ok(OsString::from_vec(self.0.clone()))
        }

        #[cfg(not(unix))]
        {
            Err(NativeOsValueConversionError::UnsupportedPlatform)
        }
    }
}

impl Serialize for OpaqueOsValueV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&OPAQUE_OS_VALUE_BASE64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for OpaqueOsValueV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        OPAQUE_OS_VALUE_BASE64
            .decode(encoded)
            .map(Self)
            .map_err(|_| D::Error::custom("invalid base64-encoded OS value"))
    }
}

/// Failure to convert native OS values on a platform without byte-exact OS-string APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NativeOsValueConversionError {
    #[error("byte-exact native OS value conversion is unsupported on this platform")]
    UnsupportedPlatform,
}

/// One environment entry. A sequence preserves ordering and repeated names.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEnvVarV1 {
    pub name: OpaqueOsValueV1,
    pub value: OpaqueOsValueV1,
}

/// Terminal dimensions captured for the native process launch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalGeometryV1 {
    pub cols: u16,
    pub rows: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

/// Version 1 of the byte-exact native process launch context.
///
/// Wire-version state is established by the type, not supplied by its caller:
///
/// ```compile_fail
/// use std::ffi::OsStr;
/// use marion_core::proto::{NativeLaunchContextV1, OpaqueOsValueV1, TerminalGeometryV1};
///
/// let value = OpaqueOsValueV1::from_os_str(OsStr::new("value")).unwrap();
/// let _invalid = NativeLaunchContextV1 {
///     wire_version: 2,
///     program: value.clone(),
///     argv: vec![],
///     cwd: value,
///     env: vec![],
///     geometry: TerminalGeometryV1 { cols: 80, rows: 24, xpixel: 0, ypixel: 0 },
/// };
/// ```
///
/// V1 has no facade selector:
///
/// ```compile_fail
/// use std::ffi::OsStr;
/// use marion_core::proto::{NativeLaunchContextV1, OpaqueOsValueV1, TerminalGeometryV1};
///
/// let value = OpaqueOsValueV1::from_os_str(OsStr::new("value")).unwrap();
/// let _invalid = NativeLaunchContextV1 {
///     facade_command: "atlas".into(),
///     program: value.clone(),
///     argv: vec![],
///     cwd: value,
///     env: vec![],
///     geometry: TerminalGeometryV1 { cols: 80, rows: 24, xpixel: 0, ypixel: 0 },
/// };
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeLaunchContextV1 {
    pub program: OpaqueOsValueV1,
    pub argv: Vec<OpaqueOsValueV1>,
    pub cwd: OpaqueOsValueV1,
    pub env: Vec<NativeEnvVarV1>,
    pub geometry: TerminalGeometryV1,
}

/// Versioned native process-launch state carried by the client↔supervisor protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeLaunchContext {
    V1(NativeLaunchContextV1),
    V2(NativeLaunchContextV2),
}

/// Version 2 of the native process launch context, bound to one canonical facade command.
///
/// The facade selector is required by semantic construction:
///
/// ```compile_fail
/// use std::ffi::OsStr;
/// use marion_core::proto::{NativeLaunchContextV2, OpaqueOsValueV1, TerminalGeometryV1};
///
/// let value = OpaqueOsValueV1::from_os_str(OsStr::new("value")).unwrap();
/// let _invalid = NativeLaunchContextV2 {
///     program: value.clone(),
///     argv: vec![],
///     cwd: value,
///     env: vec![],
///     geometry: TerminalGeometryV1 { cols: 80, rows: 24, xpixel: 0, ypixel: 0 },
/// };
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeLaunchContextV2 {
    pub facade_command: String,
    pub program: OpaqueOsValueV1,
    pub argv: Vec<OpaqueOsValueV1>,
    pub cwd: OpaqueOsValueV1,
    pub env: Vec<NativeEnvVarV1>,
    pub geometry: TerminalGeometryV1,
}

const NATIVE_LAUNCH_WIRE_VERSION: u16 = 1;
const NATIVE_LAUNCH_V2_WIRE_VERSION: u16 = 2;

impl NativeLaunchContextV1 {
    /// Build semantic launch state. The wire version is fixed by this type and is not an input.
    pub fn new(
        program: OpaqueOsValueV1,
        argv: Vec<OpaqueOsValueV1>,
        cwd: OpaqueOsValueV1,
        env: Vec<NativeEnvVarV1>,
        geometry: TerminalGeometryV1,
    ) -> Self {
        Self {
            program,
            argv,
            cwd,
            env,
            geometry,
        }
    }

    /// The only wire version this type can serialize or deserialize.
    pub const fn wire_version(&self) -> u16 {
        NATIVE_LAUNCH_WIRE_VERSION
    }
}

impl NativeLaunchContextV2 {
    /// Build semantic V2 launch state with its canonical facade selector.
    pub fn new(
        facade_command: String,
        program: OpaqueOsValueV1,
        argv: Vec<OpaqueOsValueV1>,
        cwd: OpaqueOsValueV1,
        env: Vec<NativeEnvVarV1>,
        geometry: TerminalGeometryV1,
    ) -> Self {
        Self {
            facade_command,
            program,
            argv,
            cwd,
            env,
            geometry,
        }
    }

    /// The only wire version this type can serialize.
    pub const fn wire_version(&self) -> u16 {
        NATIVE_LAUNCH_V2_WIRE_VERSION
    }
}

#[derive(Serialize)]
struct NativeLaunchContextV1WireRef<'a> {
    wire_version: u16,
    program: &'a OpaqueOsValueV1,
    argv: &'a [OpaqueOsValueV1],
    cwd: &'a OpaqueOsValueV1,
    env: &'a [NativeEnvVarV1],
    geometry: &'a TerminalGeometryV1,
}

#[derive(Serialize)]
struct NativeLaunchContextV2WireRef<'a> {
    wire_version: u16,
    facade_command: &'a str,
    program: &'a OpaqueOsValueV1,
    argv: &'a [OpaqueOsValueV1],
    cwd: &'a OpaqueOsValueV1,
    env: &'a [NativeEnvVarV1],
    geometry: &'a TerminalGeometryV1,
}

impl Serialize for NativeLaunchContextV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        NativeLaunchContextV1WireRef {
            wire_version: self.wire_version(),
            program: &self.program,
            argv: &self.argv,
            cwd: &self.cwd,
            env: &self.env,
            geometry: &self.geometry,
        }
        .serialize(serializer)
    }
}

impl Serialize for NativeLaunchContextV2 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        NativeLaunchContextV2WireRef {
            wire_version: self.wire_version(),
            facade_command: &self.facade_command,
            program: &self.program,
            argv: &self.argv,
            cwd: &self.cwd,
            env: &self.env,
            geometry: &self.geometry,
        }
        .serialize(serializer)
    }
}

impl Serialize for NativeLaunchContext {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::V1(context) => context.serialize(serializer),
            Self::V2(context) => context.serialize(serializer),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeLaunchContextV1Wire {
    wire_version: u16,
    program: OpaqueOsValueV1,
    argv: Vec<OpaqueOsValueV1>,
    cwd: OpaqueOsValueV1,
    env: Vec<NativeEnvVarV1>,
    geometry: TerminalGeometryV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeLaunchContextV2Wire {
    wire_version: u16,
    facade_command: String,
    program: OpaqueOsValueV1,
    argv: Vec<OpaqueOsValueV1>,
    cwd: OpaqueOsValueV1,
    env: Vec<NativeEnvVarV1>,
    geometry: TerminalGeometryV1,
}

#[derive(Deserialize)]
struct NativeLaunchContextDispatch {
    wire_version: u16,
}

/// JSON buffered without a key-unique map, so duplicate keys survive version selection and are
/// rejected by the selected strict wire schema, including inside nested objects.
enum NativeLaunchRawJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl Serialize for NativeLaunchRawJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
            Self::Object(fields) => {
                let mut object = serializer.serialize_map(Some(fields.len()))?;
                for (name, value) in fields {
                    object.serialize_entry(name, value)?;
                }
                object.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for NativeLaunchRawJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NativeLaunchRawJsonVisitor;

        impl<'de> Visitor<'de> for NativeLaunchRawJsonVisitor {
            type Value = NativeLaunchRawJson;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("any valid JSON value")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(NativeLaunchRawJson::Null)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(NativeLaunchRawJson::Null)
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                NativeLaunchRawJson::deserialize(deserializer)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(NativeLaunchRawJson::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(NativeLaunchRawJson::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(NativeLaunchRawJson::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(NativeLaunchRawJson::Number)
                    .ok_or_else(|| E::custom("non-finite number is not valid JSON"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(NativeLaunchRawJson::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(NativeLaunchRawJson::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(NativeLaunchRawJson::Array(values))
            }

            fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut fields = Vec::with_capacity(object.size_hint().unwrap_or(0));
                while let Some(field) = object.next_entry()? {
                    fields.push(field);
                }
                Ok(NativeLaunchRawJson::Object(fields))
            }
        }

        deserializer.deserialize_any(NativeLaunchRawJsonVisitor)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unsupported native launch wire_version {actual}; expected 1")]
struct NativeLaunchWireVersionError {
    actual: u16,
}

impl TryFrom<NativeLaunchContextV1Wire> for NativeLaunchContextV1 {
    type Error = NativeLaunchWireVersionError;

    fn try_from(wire: NativeLaunchContextV1Wire) -> Result<Self, Self::Error> {
        if wire.wire_version != 1 {
            return Err(NativeLaunchWireVersionError {
                actual: wire.wire_version,
            });
        }

        Ok(Self::new(
            wire.program,
            wire.argv,
            wire.cwd,
            wire.env,
            wire.geometry,
        ))
    }
}

impl From<NativeLaunchContextV2Wire> for NativeLaunchContextV2 {
    fn from(wire: NativeLaunchContextV2Wire) -> Self {
        let _ = wire.wire_version;
        Self::new(
            wire.facade_command,
            wire.program,
            wire.argv,
            wire.cwd,
            wire.env,
            wire.geometry,
        )
    }
}

impl<'de> Deserialize<'de> for NativeLaunchContextV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        NativeLaunchContextV1Wire::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
}

impl<'de> Deserialize<'de> for NativeLaunchContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = NativeLaunchRawJson::deserialize(deserializer)?;
        let raw = serde_json::to_vec(&raw).map_err(D::Error::custom)?;
        let dispatch = serde_json::from_slice::<NativeLaunchContextDispatch>(&raw)
            .map_err(D::Error::custom)?;
        let wire_version = dispatch.wire_version;

        match wire_version {
            1 => serde_json::from_slice::<NativeLaunchContextV1Wire>(&raw)
                .map_err(D::Error::custom)?
                .try_into()
                .map(Self::V1)
                .map_err(D::Error::custom),
            2 => serde_json::from_slice::<NativeLaunchContextV2Wire>(&raw)
                .map(NativeLaunchContextV2::from)
                .map(Self::V2)
                .map_err(D::Error::custom),
            actual => Err(D::Error::custom(format_args!(
                "unsupported native launch wire_version {actual}; expected 1 or 2"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(unix))]
    use super::NativeOsValueConversionError;
    use super::{
        NativeEnvVarV1, NativeLaunchContext, NativeLaunchContextV1, NativeLaunchContextV2,
        OpaqueOsValueV1, TerminalGeometryV1,
    };
    use crate::proto::params::AgentSpawnParams;
    use crate::proto::{Call, Frame};

    const OLD_SPAWN_FRAME: &str = r#"{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{"agent_type":"codex-impl","prompt":"go","caller":null,"repo":"/r","acceptance_criteria":[],"writable_scope":[],"timeout_secs":null,"model":null,"no_change_record":null,"pane":null,"isolation":null,"allow_concurrent_writes":null}}"#;

    fn minimal_context_json(wire_version: u16) -> String {
        format!(
            r#"{{"wire_version":{wire_version},"program":"L2Jpbi9zaA==","argv":[],"cwd":"L3RtcA==","env":[],"geometry":{{"cols":80,"rows":24,"xpixel":0,"ypixel":0}}}}"#
        )
    }

    fn minimal_v2_context_json() -> &'static str {
        r#"{"wire_version":2,"facade_command":"atlas","program":"L2Jpbi9zaA==","argv":[],"cwd":"L3RtcA==","env":[],"geometry":{"cols":80,"rows":24,"xpixel":0,"ypixel":0}}"#
    }

    fn assert_duplicate_native_context_is_rejected(case: &str, context: &str) {
        assert!(
            serde_json::from_str::<NativeLaunchContext>(context).is_err(),
            "{case}: direct native-context decoding accepted a duplicate field"
        );
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{{"agent_type":"t","prompt":"p","native_launch":{context}}}}}"#
        );
        assert!(
            Frame::from_line(&line).is_err(),
            "{case}: frame decoding accepted a duplicate native-context field"
        );
    }

    #[cfg(unix)]
    fn opaque(bytes: &[u8]) -> OpaqueOsValueV1 {
        use std::os::unix::ffi::OsStringExt;

        let value = std::ffi::OsString::from_vec(bytes.to_vec());
        OpaqueOsValueV1::from_os_str(value.as_os_str()).expect("Unix OS values are byte strings")
    }

    #[cfg(unix)]
    fn bytes(value: &OpaqueOsValueV1) -> Vec<u8> {
        use std::os::unix::ffi::OsStringExt;

        value
            .to_os_string()
            .expect("Unix OS values are byte strings")
            .into_vec()
    }

    #[cfg(unix)]
    #[test]
    fn opaque_os_values_use_padded_standard_base64() {
        let value = opaque(&[0xff]);

        assert_eq!(serde_json::to_string(&value).unwrap(), r#""/w==""#);
        assert_eq!(
            serde_json::from_str::<OpaqueOsValueV1>(r#""/w==""#).unwrap(),
            value
        );
    }

    #[test]
    fn noncanonical_base64_padding_variants_are_rejected() {
        for encoded in [r#""/w""#, r#""/w=""#, r#""/w===""#] {
            let error = serde_json::from_str::<OpaqueOsValueV1>(encoded)
                .expect_err("only canonical `/w==` may encode byte 0xff")
                .to_string();

            assert!(error.contains("base64-encoded OS value"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn public_native_context_construction_serializes_only_wire_version_one() {
        let context = NativeLaunchContextV1::new(
            opaque(b"program"),
            vec![],
            opaque(b"/cwd"),
            vec![],
            TerminalGeometryV1 {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            },
        );

        assert_eq!(context.wire_version(), 1);
        assert_eq!(
            serde_json::to_string(&context).unwrap(),
            r#"{"wire_version":1,"program":"cHJvZ3JhbQ==","argv":[],"cwd":"L2N3ZA==","env":[],"geometry":{"cols":80,"rows":24,"xpixel":0,"ypixel":0}}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn invalid_unix_bytes_survive_a_native_context_json_round_trip() {
        let context = NativeLaunchContextV1::new(
            opaque(b"/bin/tool\xff"),
            vec![opaque(b"tool\xfe"), opaque(b"--mode=\xfd")],
            opaque(b"/tmp/tree\xfc"),
            vec![NativeEnvVarV1 {
                name: opaque(b"KEY\xfb"),
                value: opaque(b"VALUE\xfa"),
            }],
            TerminalGeometryV1 {
                cols: 211,
                rows: 73,
                xpixel: 1920,
                ypixel: 1080,
            },
        );

        let json = serde_json::to_string(&context).unwrap();
        let decoded: NativeLaunchContextV1 = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, context);
        assert_eq!(bytes(&decoded.program), b"/bin/tool\xff");
        assert_eq!(bytes(&decoded.cwd), b"/tmp/tree\xfc");
    }

    #[cfg(unix)]
    #[test]
    fn argv_and_env_keep_order_boundaries_and_empty_values() {
        let context = NativeLaunchContextV1::new(
            opaque(b"program"),
            vec![
                opaque(b""),
                opaque(b"ab"),
                opaque(b"c"),
                opaque(b"a"),
                opaque(b"bc"),
            ],
            opaque(b"/cwd"),
            vec![
                NativeEnvVarV1 {
                    name: opaque(b"FIRST"),
                    value: opaque(b""),
                },
                NativeEnvVarV1 {
                    name: opaque(b"SECOND\xff"),
                    value: opaque(b"two"),
                },
            ],
            TerminalGeometryV1 {
                cols: 0,
                rows: u16::MAX,
                xpixel: 1,
                ypixel: u16::MAX - 1,
            },
        );

        let decoded: NativeLaunchContextV1 =
            serde_json::from_str(&serde_json::to_string(&context).unwrap()).unwrap();
        let argv: Vec<Vec<u8>> = decoded.argv.iter().map(bytes).collect();
        let env: Vec<(Vec<u8>, Vec<u8>)> = decoded
            .env
            .iter()
            .map(|entry| (bytes(&entry.name), bytes(&entry.value)))
            .collect();

        assert_eq!(
            argv,
            [
                b"".to_vec(),
                b"ab".to_vec(),
                b"c".to_vec(),
                b"a".to_vec(),
                b"bc".to_vec()
            ]
        );
        assert_eq!(
            env,
            [
                (b"FIRST".to_vec(), b"".to_vec()),
                (b"SECOND\xff".to_vec(), b"two".to_vec()),
            ]
        );
        assert_eq!(decoded.geometry, context.geometry);
    }

    #[test]
    fn malformed_base64_is_rejected_without_echoing_the_value() {
        let secret = "DO_NOT_ECHO_ARBITRARY_ENV_CONTENT!";
        let json = format!(r#""{secret}""#);

        let error = serde_json::from_str::<OpaqueOsValueV1>(&json)
            .unwrap_err()
            .to_string();

        assert!(error.contains("base64-encoded OS value"), "{error}");
        assert!(
            !error.contains(secret),
            "decode errors must not echo values: {error}"
        );
    }

    #[test]
    fn direct_v1_decoder_refuses_wire_version_two() {
        assert!(serde_json::from_str::<NativeLaunchContextV1>(&minimal_context_json(2)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn v1_context_keeps_canonical_bytes_and_accepts_noncanonical_json() {
        use std::ffi::OsStr;

        let opaque = |value: &str| {
            OpaqueOsValueV1::from_os_str(OsStr::new(value))
                .expect("Unix OS values are byte strings")
        };
        let semantic_v1 = NativeLaunchContext::V1(NativeLaunchContextV1::new(
            opaque("/bin/probe"),
            vec![opaque("--foo")],
            opaque("/tmp"),
            vec![],
            TerminalGeometryV1 {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            },
        ));

        let canonical_once = serde_json::to_vec(&semantic_v1).unwrap();
        let decoded = serde_json::from_slice::<NativeLaunchContext>(&canonical_once).unwrap();
        let canonical_twice = serde_json::to_vec(&decoded).unwrap();
        assert_eq!(decoded, semantic_v1);
        assert_eq!(canonical_twice, canonical_once);

        let accepted_noncanonical = br#"{
  "geometry": { "ypixel": 0, "xpixel": 0, "rows": 24, "cols": 80 },
  "env": [],
  "cwd": "L3RtcA==",
  "argv": [ "LS1mb28=" ],
  "program": "L2Jpbi9wcm9iZQ==",
  "wire_version": 1
}"#;
        assert_eq!(
            serde_json::from_slice::<NativeLaunchContext>(accepted_noncanonical).unwrap(),
            semantic_v1,
        );
    }

    #[cfg(unix)]
    #[test]
    fn v2_carries_the_canonical_selector_and_opaque_values() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let opaque = |bytes: &[u8]| {
            OpaqueOsValueV1::from_os_str(OsStr::from_bytes(bytes))
                .expect("Unix OS values are byte strings")
        };
        let context = NativeLaunchContext::V2(NativeLaunchContextV2::new(
            "atlas".into(),
            opaque(b"/probe/atlas-cli"),
            vec![opaque(&[0xff, b'x'])],
            opaque(&[b'/', b'w', 0x80]),
            vec![NativeEnvVarV1 {
                name: opaque(b"PATH"),
                value: opaque(&[b'/', b'p', 0x81]),
            }],
            TerminalGeometryV1 {
                cols: 91,
                rows: 37,
                xpixel: 0,
                ypixel: 0,
            },
        ));

        let json = serde_json::to_string(&context).unwrap();
        assert!(json.contains(r#""wire_version":2"#));
        assert!(json.contains(r#""facade_command":"atlas""#));
        assert_eq!(
            serde_json::from_str::<NativeLaunchContext>(&json).unwrap(),
            context
        );
        assert!(serde_json::from_str::<NativeLaunchContextV1>(&json).is_err());
    }

    #[test]
    fn native_context_dispatch_is_strict_per_known_version() {
        let unknown_v1 = format!(
            "{},\"facade_command\":\"atlas\"}}",
            minimal_context_json(1)
                .strip_suffix('}')
                .expect("the fixture is an object")
        );
        assert!(serde_json::from_str::<NativeLaunchContext>(&unknown_v1).is_err());

        let missing_v2 = minimal_context_json(2);
        assert!(serde_json::from_str::<NativeLaunchContext>(&missing_v2).is_err());

        let unknown_v2 =
            minimal_v2_context_json().replacen(r#""geometry""#, r#""surprise":true,"geometry""#, 1);
        assert!(serde_json::from_str::<NativeLaunchContext>(&unknown_v2).is_err());
    }

    #[test]
    fn duplicate_native_wire_version_is_rejected() {
        let json = r#"{"wire_version":1,"wire_version":1,"program":"L2Jpbi9zaA==","argv":[],"cwd":"L3RtcA==","env":[],"geometry":{"cols":80,"rows":24,"xpixel":0,"ypixel":0}}"#;

        assert_duplicate_native_context_is_rejected("wire_version", json);
    }

    #[test]
    fn duplicate_native_v1_program_is_rejected() {
        let json = r#"{"wire_version":1,"program":"L2Jpbi9zaA==","program":"L2Jpbi9iYXNo","argv":[],"cwd":"L3RtcA==","env":[],"geometry":{"cols":80,"rows":24,"xpixel":0,"ypixel":0}}"#;

        assert_duplicate_native_context_is_rejected("V1 program", json);
    }

    #[test]
    fn duplicate_native_v2_facade_command_is_rejected() {
        let json = r#"{"wire_version":2,"facade_command":"atlas","facade_command":"codex","program":"L2Jpbi9zaA==","argv":[],"cwd":"L3RtcA==","env":[],"geometry":{"cols":80,"rows":24,"xpixel":0,"ypixel":0}}"#;

        assert_duplicate_native_context_is_rejected("V2 facade_command", json);
    }

    #[test]
    fn duplicate_native_nested_geometry_field_is_rejected() {
        let json = r#"{"wire_version":2,"facade_command":"atlas","program":"L2Jpbi9zaA==","argv":[],"cwd":"L3RtcA==","env":[],"geometry":{"cols":80,"cols":81,"rows":24,"xpixel":0,"ypixel":0}}"#;

        assert_duplicate_native_context_is_rejected("nested geometry.cols", json);
    }

    #[test]
    fn unknown_native_context_versions_never_fall_through_to_known_schemas() {
        for version in [0_u16, 3, u16::MAX] {
            let json = minimal_v2_context_json().replacen(
                r#""wire_version":2"#,
                &format!(r#""wire_version":{version}"#),
                1,
            );
            assert!(
                serde_json::from_str::<NativeLaunchContext>(&json).is_err(),
                "wire version {version} must not dispatch"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn boxing_native_context_does_not_change_agent_spawn_json() {
        let context = NativeLaunchContextV1::new(
            opaque(b"/bin/sh"),
            vec![],
            opaque(b"/tmp"),
            vec![],
            TerminalGeometryV1 {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            },
        );
        let expected = serde_json::json!({
            "wire_version": 1,
            "program": "L2Jpbi9zaA==",
            "argv": [],
            "cwd": "L3RtcA==",
            "env": [],
            "geometry": { "cols": 80, "rows": 24, "xpixel": 0, "ypixel": 0 },
        });
        let params = AgentSpawnParams {
            agent_type: "t".into(),
            prompt: "p".into(),
            native_launch: Some(Box::new(NativeLaunchContext::V1(context))),
            caller: None,
            repo: None,
            acceptance_criteria: vec![],
            writable_scope: vec![],
            timeout_secs: None,
            model: None,
            no_change_record: None,
            pane: None,
            isolation: None,
            allow_concurrent_writes: None,
        };

        assert_eq!(
            serde_json::to_value(params).unwrap()["native_launch"],
            expected
        );
    }

    #[test]
    fn unknown_native_schema_fields_are_refused_at_every_object_boundary() {
        let cases = [
            (
                serde_json::from_str::<NativeEnvVarV1>(
                    r#"{"name":"S0VZ","value":"VkFMVUU=","extra":true}"#,
                )
                .unwrap_err()
                .to_string(),
                "extra",
            ),
            (
                serde_json::from_str::<TerminalGeometryV1>(
                    r#"{"cols":80,"rows":24,"xpixel":0,"ypixel":0,"depth":32}"#,
                )
                .unwrap_err()
                .to_string(),
                "depth",
            ),
            (
                serde_json::from_str::<NativeLaunchContextV1>(&format!(
                    "{},\"surprise\":true}}",
                    minimal_context_json(1)
                        .strip_suffix('}')
                        .expect("the fixture is an object")
                ))
                .unwrap_err()
                .to_string(),
                "surprise",
            ),
        ];

        for (error, field) in cases {
            assert!(
                error.contains(field),
                "rejection must name {field}: {error}"
            );
        }
    }

    #[test]
    fn an_old_spawn_frame_deserializes_and_reserializes_byte_for_byte() {
        let frame = Frame::from_line(OLD_SPAWN_FRAME).unwrap();
        let Frame::Request(request) = &frame else {
            panic!("old spawn frame parsed as the wrong frame kind");
        };
        let Call::AgentSpawn(params) = &request.call else {
            panic!("old spawn frame parsed as the wrong method");
        };

        assert_eq!(params.native_launch, None);
        assert_eq!(frame.to_line(), format!("{OLD_SPAWN_FRAME}\n"));
    }

    #[test]
    fn a_v1_native_spawn_frame_has_one_canonical_wire_shape() {
        let context = minimal_context_json(1);
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{{"agent_type":"t","prompt":"p","native_launch":{context}}}}}"#
        );

        let frame = Frame::from_line(&line).expect("the V1 native spawn frame is valid");

        assert_eq!(
            frame.to_line(),
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{"agent_type":"t","prompt":"p","native_launch":{"wire_version":1,"program":"L2Jpbi9zaA==","argv":[],"cwd":"L3RtcA==","env":[],"geometry":{"cols":80,"rows":24,"xpixel":0,"ypixel":0}},"caller":null,"repo":null,"acceptance_criteria":[],"writable_scope":[],"timeout_secs":null,"model":null,"no_change_record":null,"pane":null,"isolation":null,"allow_concurrent_writes":null}}"#,
                "\n"
            )
        );
    }

    #[test]
    fn a_spawn_frame_with_an_unknown_native_field_is_refused_loudly() {
        let context = format!(
            "{},\"surprise\":true}}",
            minimal_context_json(1)
                .strip_suffix('}')
                .expect("the fixture is an object")
        );
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{{"agent_type":"t","prompt":"p","native_launch":{context}}}}}"#
        );

        let error = Frame::from_line(&line).unwrap_err();

        assert!(error.message.contains("agent/spawn"), "{error}");
        assert!(error.message.contains("surprise"), "{error}");
    }

    #[test]
    fn a_spawn_frame_with_v2_but_no_selector_is_refused_loudly() {
        let context = minimal_context_json(2);
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{{"agent_type":"t","prompt":"p","native_launch":{context}}}}}"#
        );

        let error = Frame::from_line(&line).unwrap_err();

        assert!(error.message.contains("agent/spawn"), "{error}");
        assert!(error.message.contains("facade_command"), "{error}");
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_os_value_conversion_returns_a_typed_error() {
        let error = OpaqueOsValueV1::from_os_str(std::ffi::OsStr::new("value")).unwrap_err();

        assert_eq!(error, NativeOsValueConversionError::UnsupportedPlatform);
    }
}
