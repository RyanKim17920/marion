//! Errors that carry a sentence, a kind and a citation — not just a number.
//!
//! The rule this type exists to serve: **a refusal must be distinguishable from a thing that does
//! not work.** Those two are the same JSON-RPC shape and, in every protocol that stops at a code,
//! the same operational experience: a client shows "error" and the operator cannot tell whether
//! marion decided against them or fell over. They call for opposite reactions — a refusal is
//! answered by changing the request, a malfunction by looking at the supervisor — so they are
//! separated three ways here, each independently checkable:
//!
//! 1. **By code.** [`FailureKind::code`] is injective (pinned by test), so a peer that reads
//!    nothing but `code` can still tell a refusal from an internal error.
//! 2. **By kind.** [`ErrorData::kind`] names which of the six it is, for a peer that wants to
//!    branch without a code table.
//! 3. **By sentence.** `message` says what happened in words, and no constructor here can produce
//!    an error without one.
//!
//! `spec` is the fourth part and the one most likely to be dismissed as decoration. It is not: a
//! refusal in this system is nearly always a *specified* refusal — §5.4's target-state predicate,
//! §7.2's reap exclusions, §11 item 23's unimplemented parameters — and the operator's next
//! question is always "says who". Answering it in the error costs a `&'static str`.

use serde::{Deserialize, Serialize};

/// A JSON-RPC 2.0 error object.
///
/// `code` is a bare `i32` rather than an enum because the reserved range is open-ended and a peer
/// may legitimately send a code this build has never heard of; narrowing it would turn a
/// forward-compatible error into a parse failure, which is the one thing an error type must never
/// do. [`FailureKind`] is the closed set marion itself produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct RpcError {
    pub code: i32,
    /// A sentence. Never a bare identifier, never an empty string — every constructor below takes
    /// one and there is no `Default`.
    pub message: String,
    /// Absent when the error came from a peer that does not speak marion's extension. Its absence
    /// is why [`RpcError::kind`] returns an `Option` rather than guessing from the code: guessing
    /// would invent a classification the sender never made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorData>,
}

/// marion's extension to the JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorData {
    pub kind: FailureKind,
    /// What the error is *about* — an `AgentId`, a method name, a parameter name. Separate from
    /// `message` so a client can group ten refusals about one node without parsing prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The design section that specifies this refusal, e.g. `"§5.4"`. Empty only for the JSON-RPC
    /// framing errors, which no marion section specifies because JSON-RPC does.
    pub spec: String,
}

/// The six things that can go wrong, separated by **what the caller should do next**.
///
/// That is the axis, not severity and not HTTP-like semantics. `Refused` and `Unsupported` look
/// alike and are not: a refusal would have succeeded under different conditions the caller can
/// often create (finish the turn, answer the permission), whereas an unsupported call will never
/// succeed against this node because the capability is absent (§3.3 — *"the UI greys out an action
/// iff its capability is false"*, and greying out is precisely the client behaviour `Unsupported`
/// should drive). Collapsing them would make the client retry forever against a harness that
/// cannot steer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    /// marion decided against it, and could decide otherwise later.
    Refused,
    /// The capability is absent (§3.3). Retrying changes nothing.
    Unsupported,
    /// No such node, request or agent type.
    NotFound,
    /// The call was valid and lost a race — a permission already answered, a contract already
    /// reported (§5.4: *"first call per `TaskContract` wins"*).
    Conflict,
    /// Specified, not built. Distinct from `Unsupported` because the gap is marion's, not the
    /// harness's, and the operator's next move is to check the milestone rather than the node.
    Unimplemented,
    /// marion is broken. The only kind that is not the caller's business to fix.
    Internal,
}

impl FailureKind {
    /// Injective by test. `Internal` reuses JSON-RPC's reserved `-32603` because that is exactly
    /// what it means; every other kind takes an application code from the `-32000..=-32099`
    /// implementation-defined range, so none of them can be mistaken for a framing failure.
    pub const fn code(self) -> i32 {
        match self {
            FailureKind::Refused => -32001,
            FailureKind::Unsupported => -32002,
            FailureKind::NotFound => -32003,
            FailureKind::Conflict => -32004,
            FailureKind::Unimplemented => -32005,
            FailureKind::Internal => -32603,
        }
    }

    pub const ALL: [FailureKind; 6] = [
        FailureKind::Refused,
        FailureKind::Unsupported,
        FailureKind::NotFound,
        FailureKind::Conflict,
        FailureKind::Unimplemented,
        FailureKind::Internal,
    ];
}

/// JSON-RPC 2.0's own reserved codes, for the three failures that happen before a method is known.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;

impl RpcError {
    /// The one constructor for a marion-classified error. Takes the sentence and the citation
    /// because an error missing either is the error this module exists to prevent.
    pub fn of(
        kind: FailureKind,
        subject: Option<&str>,
        message: impl Into<String>,
        spec: &str,
    ) -> Self {
        Self {
            code: kind.code(),
            message: message.into(),
            data: Some(ErrorData {
                kind,
                subject: subject.map(str::to_owned),
                spec: spec.to_owned(),
            }),
        }
    }

    pub fn refused(subject: &str, message: impl Into<String>, spec: &str) -> Self {
        Self::of(FailureKind::Refused, Some(subject), message, spec)
    }

    pub fn unsupported(subject: &str, message: impl Into<String>, spec: &str) -> Self {
        Self::of(FailureKind::Unsupported, Some(subject), message, spec)
    }

    pub fn not_found(subject: &str, message: impl Into<String>, spec: &str) -> Self {
        Self::of(FailureKind::NotFound, Some(subject), message, spec)
    }

    pub fn conflict(subject: &str, message: impl Into<String>, spec: &str) -> Self {
        Self::of(FailureKind::Conflict, Some(subject), message, spec)
    }

    pub fn unimplemented(subject: &str, message: impl Into<String>, spec: &str) -> Self {
        Self::of(FailureKind::Unimplemented, Some(subject), message, spec)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::of(FailureKind::Internal, None, message, "")
    }

    /// A line that is not JSON at all. Carries no `data`: nothing has been classified yet, and
    /// claiming a `FailureKind` for a byte sequence marion could not read would be a guess.
    pub fn parse(message: impl Into<String>) -> Self {
        Self::framing(PARSE_ERROR, message)
    }

    /// Valid JSON that is not a JSON-RPC frame.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::framing(INVALID_REQUEST, message)
    }

    pub fn method_not_found(message: impl Into<String>) -> Self {
        Self::framing(METHOD_NOT_FOUND, message)
    }

    /// A known method whose `params` did not deserialize — including the unknown-field rejection
    /// that `deny_unknown_fields` performs, which is why this is a framing code rather than a
    /// `Refused`: the request never became a call.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::framing(INVALID_PARAMS, message)
    }

    fn framing(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// `None` when the sender did not classify — see [`RpcError::data`].
    pub fn kind(&self) -> Option<FailureKind> {
        self.data.as_ref().map(|d| d.kind)
    }

    /// The question a client actually asks: *is this my fault or marion's?*
    pub fn is_refusal(&self) -> bool {
        matches!(
            self.kind(),
            Some(
                FailureKind::Refused
                    | FailureKind::Unsupported
                    | FailureKind::NotFound
                    | FailureKind::Conflict
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_kind_codes_are_injective() {
        // If two kinds shared a code, a peer reading only `code` could not tell a refusal from a
        // malfunction — which is the whole property this module claims.
        let mut codes: Vec<i32> = FailureKind::ALL.iter().map(|k| k.code()).collect();
        codes.sort_unstable();
        let n = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), n, "two FailureKinds share a code");
    }

    #[test]
    fn a_refusal_is_distinguishable_from_a_malfunction() {
        let refused =
            RpcError::refused("agent-1", "node is running; use node/steer (§6.3)", "§6.3");
        let broke = RpcError::internal("registry lock poisoned");
        assert!(refused.is_refusal());
        assert!(!broke.is_refusal());
        assert_ne!(refused.code, broke.code);
        assert_ne!(refused.kind(), broke.kind());
    }

    #[test]
    fn every_constructor_produces_a_sentence() {
        let errs = [
            RpcError::refused("a", "the node is not idle", "§6.3"),
            RpcError::unsupported("a", "this harness cannot steer", "§3.3"),
            RpcError::not_found("a", "no node with that id", "§3.2"),
            RpcError::conflict("a", "that permission was already answered", "§5.6"),
            RpcError::unimplemented("a", "policy/set names no policy", "§2"),
            RpcError::internal("the registry lock is poisoned"),
            RpcError::parse("expected value at line 1"),
            RpcError::invalid_request("a frame carries either a method or an id"),
            RpcError::method_not_found("no such method: node/frobnicate"),
            RpcError::invalid_params("unknown field `backgrund`"),
        ];
        for e in &errs {
            assert!(!e.message.is_empty(), "{e:?} has no sentence");
            assert!(e.message.len() > 5, "{e:?} is not a sentence");
        }
    }

    #[test]
    fn framing_errors_do_not_claim_a_classification() {
        // A byte sequence marion could not read has no kind, and inventing one would be a guess
        // the sender never made.
        assert_eq!(RpcError::parse("bad json").kind(), None);
        assert!(!RpcError::parse("bad json").is_refusal());
    }

    #[test]
    fn error_pins_its_wire_shape() {
        let e = RpcError::refused("agent-1", "the node is running", "§6.3");
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"code":-32001,"message":"the node is running","data":{"kind":"Refused","subject":"agent-1","spec":"§6.3"}}"#
        );
        let back: RpcError = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn a_framing_error_omits_data_entirely() {
        assert_eq!(
            serde_json::to_string(&RpcError::parse("expected value")).unwrap(),
            r#"{"code":-32700,"message":"expected value"}"#
        );
    }

    #[test]
    fn an_error_from_a_peer_without_data_still_parses() {
        // Forward compatibility in the direction that matters: a peer that does not speak marion's
        // extension must still be readable, or the error type becomes the outage.
        let e: RpcError = serde_json::from_str(r#"{"code":-32000,"message":"something"}"#).unwrap();
        assert_eq!(e.kind(), None);
        assert_eq!(e.code, -32000);
    }
}
