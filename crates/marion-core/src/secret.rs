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
//! Serialization is `#[serde(transparent)]`: a secret that has to cross a wire (a node token in
//! JSON-RPC params, a key in a harness's config document) is the same bare JSON string it was
//! before it had a type, so every pinned wire shape is unchanged.

use serde::{Deserialize, Serialize};

/// A key, token or bearer. `Debug` prints `Secret(***)`; there is no `Display`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

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

/// Compares every byte of equal-length values rather than stopping at the first difference, so
/// the time a comparison takes does not say how much of a presented token was right. The length
/// itself is not hidden; marion's tokens are fixed-length.
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

    #[test]
    fn serializes_as_the_bare_string_it_was_before_it_had_a_type() {
        let json = serde_json::to_string(&Secret::new(PLAIN)).unwrap();
        assert_eq!(json, serde_json::to_string(PLAIN).unwrap());
        let back: Secret = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Secret::new(PLAIN));
        assert!(serde_json::from_str::<Secret>("7").is_err());
    }

    #[test]
    fn equality_is_by_value() {
        assert_eq!(Secret::new("abc"), Secret::new("abc"));
        assert_ne!(Secret::new("abc"), Secret::new("abd"));
        assert_ne!(Secret::new("abc"), Secret::new("abcd"));
        assert_ne!(Secret::new(""), Secret::new("a"));
        assert_eq!(Secret::new(""), Secret::new(""));
    }
}
