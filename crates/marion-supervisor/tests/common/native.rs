//! **Small readings the native-facade suites share**: a launch value as the wire carries it.

use std::ffi::OsStr;

use marion_core::proto::OpaqueOsValueV1;

/// `value` as a native launch context carries it. Infallible on Unix, which preserves the bytes.
pub fn opaque(value: &OsStr) -> OpaqueOsValueV1 {
    OpaqueOsValueV1::from_os_str(value).expect("Unix preserves native launch bytes")
}
