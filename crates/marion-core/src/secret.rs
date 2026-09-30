//! [`Secret`]: the one type every key, token and bearer marion holds is kept in.
//!
//! A secret held as a plain `String` inside a struct that derives `Debug` is printed by every
//! `{:?}` that reaches it — a panic message, a trace line, an `assert_eq!` failure, an error whose
//! `Display` embeds a `Debug`. Wrapping it makes that structural rather than a matter of care:
//!
//! * `Debug` prints `Secret(***)`, whatever the value, so a struct can keep deriving `Debug`;
//! * there is **no** `Display`, so a secret cannot be `format!`ed by accident;
//! * the plaintext comes out only through [`Secret::expose`], a name a reviewer (and the security
//!   check) can find at every place a secret leaves marion.
//!
//! * there is **no** `Serialize`, so a struct holding a secret cannot be written to a record, a
//!   log or a reply by deriving it. A field that must carry one across a wire (a node token in
//!   JSON-RPC params) says so with `#[serde(serialize_with = "…::serialize_exposed")]` — the
//!   explicit wrapper, and a name the security check finds like [`Secret::expose`]. The value is
//!   the same bare JSON string it was before it had a type, so every pinned wire shape is
//!   unchanged. Deserializing is always allowed: reading a secret in exposes nothing.

use serde::Deserialize;

/// A key, token or bearer. `Debug` prints `Secret(***)`; there is no `Display` and no `Serialize`.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

/// **The one way a secret is serialized**: `#[serde(serialize_with = "serialize_exposed")]` on a
/// field that must carry its plaintext across a wire, as a bare string.
pub fn serialize_exposed<S: serde::Serializer>(s: &Secret, ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&s.0)
}

/// [`serialize_exposed`] for an optional secret: absent is `null`, as it always was.
pub fn serialize_exposed_opt<S: serde::Serializer>(
    s: &Option<Secret>,
    ser: S,
) -> Result<S::Ok, S::Error> {
    match s {
        Some(s) => ser.serialize_some(&s.0),
        None => ser.serialize_none(),
    }
}

impl Secret {
    /// Wraps a value marion minted or was handed. No validation: what a valid key looks like is
    /// the business of whoever accepts one (`marion login` checks its own grammar).
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The plaintext, for the one place that hands it on — a child's environment, a `0600`
    /// config document, a request header. Never for a message, a log line or a record.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether there is a value at all — a presence bit, which says nothing about the value.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Secret(value.to_string())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// **Constant-time for equal lengths**, because this is the comparison the supervisor checks a
/// presented node token with. `==` on `String` returns at the first differing byte, so a caller
/// who can call `agent/spawn` repeatedly would learn a token one byte at a time; the whole slice
/// is always read instead. The length is not a secret — marion's tokens are a constant 64 hex
/// characters — so comparing it first leaks nothing and is what makes the loop a fixed-width fold.
impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0.as_bytes(), other.0.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

impl Eq for Secret {}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &str = "sk-SENTINEL-4f1d";

    #[test]
    fn debug_never_prints_the_value() {
        let s = Secret::new(PLAIN);
        let printed = format!("{s:?} {s:#?} {:?}", Some(&s));
        assert!(!printed.contains("SENTINEL"), "{printed}");
        assert!(printed.contains("Secret(***)"), "{printed}");
    }

    #[test]
    fn expose_hands_back_the_exact_value() {
        assert_eq!(Secret::new(PLAIN).expose(), PLAIN);
        assert_eq!(Secret::from(PLAIN.to_string()).expose(), PLAIN);
        assert_eq!(Secret::from(PLAIN).expose(), PLAIN);
    }

    /// **A secret serializes only through the explicit wrapper**, and there as the bare string it
    /// was before it had a type. `Secret` itself has no `Serialize`, so a struct that derives it
    /// around a secret without saying so does not compile.
    #[test]
    fn serializes_only_through_the_explicit_wrapper_as_the_bare_string() {
        #[derive(serde::Serialize)]
        struct Wire {
            #[serde(serialize_with = "serialize_exposed")]
            token: Secret,
            #[serde(serialize_with = "serialize_exposed_opt")]
            maybe: Option<Secret>,
            #[serde(serialize_with = "serialize_exposed_opt")]
            none: Option<Secret>,
        }
        let json = serde_json::to_value(Wire {
            token: Secret::new(PLAIN),
            maybe: Some(Secret::new(PLAIN)),
            none: None,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"token": PLAIN, "maybe": PLAIN, "none": null})
        );
        let back: Secret = serde_json::from_str(&serde_json::to_string(PLAIN).unwrap()).unwrap();
        assert_eq!(back, Secret::new(PLAIN));
        assert!(serde_json::from_str::<Secret>("7").is_err());
    }

    /// Asserted as correctness over every boundary a short-circuit would also get right: the
    /// timing property is not measurable in a unit test, so the fold is what a reader must not
    /// "simplify" back to `==`.
    #[test]
    fn equality_is_by_value_over_the_whole_slice() {
        assert_eq!(Secret::new("abc"), Secret::new("abc"));
        assert_ne!(Secret::new("abc"), Secret::new("abd"));
        assert_ne!(
            Secret::new("abc"),
            Secret::new("bbc"),
            "a differing first byte"
        );
        assert_ne!(
            Secret::new("abc"),
            Secret::new("abcd"),
            "a longer candidate"
        );
        assert_ne!(
            Secret::new("abcd"),
            Secret::new("abc"),
            "a shorter candidate"
        );
        // The empty token is what the supervisor mints when `/dev/urandom` cannot be read; it
        // matches only the empty string, which a `SpawnCaller` cannot carry.
        assert_ne!(Secret::new(""), Secret::new("a"));
        assert_eq!(Secret::new(""), Secret::new(""));
    }
}
