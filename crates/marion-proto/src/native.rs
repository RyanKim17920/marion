//! Exact native process-launch values carried by the client↔supervisor protocol.

use std::ffi::{OsStr, OsString};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
        serializer.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for OpaqueOsValueV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        STANDARD
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeLaunchContextV1 {
    pub wire_version: u16,
    pub program: OpaqueOsValueV1,
    pub argv: Vec<OpaqueOsValueV1>,
    pub cwd: OpaqueOsValueV1,
    pub env: Vec<NativeEnvVarV1>,
    pub geometry: TerminalGeometryV1,
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

        Ok(Self {
            wire_version: wire.wire_version,
            program: wire.program,
            argv: wire.argv,
            cwd: wire.cwd,
            env: wire.env,
            geometry: wire.geometry,
        })
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

#[cfg(test)]
mod tests {
    #[cfg(not(unix))]
    use super::NativeOsValueConversionError;
    use super::{NativeEnvVarV1, NativeLaunchContextV1, OpaqueOsValueV1, TerminalGeometryV1};
    use crate::{Call, Frame};

    const OLD_SPAWN_FRAME: &str = r#"{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{"agent_type":"codex-impl","prompt":"go","caller":null,"repo":"/r","acceptance_criteria":[],"writable_scope":[],"timeout_secs":null,"model":null,"no_change_record":null,"pane":null,"isolation":null,"allow_concurrent_writes":null}}"#;

    fn minimal_context_json(wire_version: u16) -> String {
        format!(
            r#"{{"wire_version":{wire_version},"program":"L2Jpbi9zaA==","argv":[],"cwd":"L3RtcA==","env":[],"geometry":{{"cols":80,"rows":24,"xpixel":0,"ypixel":0}}}}"#
        )
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
        let value = opaque(&[0xfb, 0xff, 0xff]);

        assert_eq!(serde_json::to_string(&value).unwrap(), r#""+///""#);
        assert_eq!(
            serde_json::from_str::<OpaqueOsValueV1>(r#""+///""#).unwrap(),
            value
        );
    }

    #[cfg(unix)]
    #[test]
    fn invalid_unix_bytes_survive_a_native_context_json_round_trip() {
        let context = NativeLaunchContextV1 {
            wire_version: 1,
            program: opaque(b"/bin/tool\xff"),
            argv: vec![opaque(b"tool\xfe"), opaque(b"--mode=\xfd")],
            cwd: opaque(b"/tmp/tree\xfc"),
            env: vec![NativeEnvVarV1 {
                name: opaque(b"KEY\xfb"),
                value: opaque(b"VALUE\xfa"),
            }],
            geometry: TerminalGeometryV1 {
                cols: 211,
                rows: 73,
                xpixel: 1920,
                ypixel: 1080,
            },
        };

        let json = serde_json::to_string(&context).unwrap();
        let decoded: NativeLaunchContextV1 = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, context);
        assert_eq!(bytes(&decoded.program), b"/bin/tool\xff");
        assert_eq!(bytes(&decoded.cwd), b"/tmp/tree\xfc");
    }

    #[cfg(unix)]
    #[test]
    fn argv_and_env_keep_order_boundaries_and_empty_values() {
        let context = NativeLaunchContextV1 {
            wire_version: 1,
            program: opaque(b"program"),
            argv: vec![
                opaque(b""),
                opaque(b"ab"),
                opaque(b"c"),
                opaque(b"a"),
                opaque(b"bc"),
            ],
            cwd: opaque(b"/cwd"),
            env: vec![
                NativeEnvVarV1 {
                    name: opaque(b"FIRST"),
                    value: opaque(b""),
                },
                NativeEnvVarV1 {
                    name: opaque(b"SECOND\xff"),
                    value: opaque(b"two"),
                },
            ],
            geometry: TerminalGeometryV1 {
                cols: 0,
                rows: u16::MAX,
                xpixel: 1,
                ypixel: u16::MAX - 1,
            },
        };

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
    fn native_launch_wire_version_two_is_refused() {
        let error = serde_json::from_str::<NativeLaunchContextV1>(&minimal_context_json(2))
            .unwrap_err()
            .to_string();

        assert!(error.contains("wire_version"), "{error}");
        assert!(error.contains('1'), "{error}");
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
    fn a_spawn_frame_with_native_wire_version_two_is_refused_loudly() {
        let context = minimal_context_json(2);
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{{"agent_type":"t","prompt":"p","native_launch":{context}}}}}"#
        );

        let error = Frame::from_line(&line).unwrap_err();

        assert!(error.message.contains("agent/spawn"), "{error}");
        assert!(error.message.contains("wire_version"), "{error}");
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_os_value_conversion_returns_a_typed_error() {
        let error = OpaqueOsValueV1::from_os_str(std::ffi::OsStr::new("value")).unwrap_err();

        assert_eq!(error, NativeOsValueConversionError::UnsupportedPlatform);
    }
}
