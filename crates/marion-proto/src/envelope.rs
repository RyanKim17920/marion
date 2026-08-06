//! The transport-independent envelope: JSON-RPC 2.0 over NDJSON.
//!
//! Transport-independent means this module knows about **lines**, not sockets: [`Frame::to_line`]
//! produces one and [`Frame::from_line`] consumes one, and whether that line arrived over a unix
//! socket (§2), a pipe, or a test fixture is not this crate's business. That is what lets the
//! transport change later be small, and what lets every frame in this file be tested without one.
//!
//! # Three framing decisions, each of which could have gone the other way
//!
//! **`jsonrpc` is validated, not skipped.** [`JsonRpcVersion`] rejects anything but `"2.0"`. The
//! tempting shortcut — serialize it, ignore it on the way in — makes marion silently accept a peer
//! speaking a protocol it does not implement, and JSON-RPC 1.0 differs in exactly the places that
//! matter here (`error` semantics, notification shape). Refusing costs one comparison.
//!
//! **A `null` `id` is rejected.** JSON-RPC 2.0 permits it and deprecates it. marion refuses it
//! outright because a response carrying `id: null` cannot be correlated with any request, and an
//! uncorrelatable response is worse than a parse error: the client's pending map keeps waiting
//! while the answer is discarded. See [`Frame::from_line`], where the check is by name so the error
//! says which key was wrong.
//!
//! **Frames are classified by key presence, not by `#[serde(untagged)]`.** Untagged would work and
//! would report every malformed line as *"data did not match any variant"* — a sentence that tells
//! an operator nothing about which of the three it nearly was. Here the classification is a
//! two-bit decision on `method` and `id`, each outcome has its own message, and an unknown method
//! is named in the refusal rather than disappearing into a variant mismatch.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::RpcError;
use crate::method::Call;
use crate::notify::Event;

/// The literal `"2.0"`, as a type — so that "did we check the version" is not a question anyone has
/// to ask about a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JsonRpcVersion;

/// The only accepted value.
pub const JSONRPC_VERSION: &str = "2.0";

impl Serialize for JsonRpcVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(JSONRPC_VERSION)
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == JSONRPC_VERSION {
            Ok(JsonRpcVersion)
        } else {
            Err(serde::de::Error::custom(format!(
                "jsonrpc must be \"{JSONRPC_VERSION}\"; this frame says {s:?}"
            )))
        }
    }
}

/// A correlation id: string or number, never null.
///
/// Untagged, because JSON-RPC's `id` genuinely is either — a client may number its requests or name
/// them, and marion echoes back whatever it was given rather than imposing a form. Numbers are
/// `i64` rather than `u64`: JSON-RPC does not forbid a negative id and a client that uses one
/// should get its own id back, not a parse error.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Text(String),
}

/// A client→supervisor request.
///
/// `call` is flattened, so the wire is JSON-RPC's flat `{"jsonrpc","id","method","params"}` rather
/// than a nested object. Flattening also means `#[serde(deny_unknown_fields)]` cannot be applied
/// here — serde forbids the combination — which is consistent with the crate's decision anyway:
/// strictness belongs on `params`, where an unknown key changes what runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    #[serde(flatten)]
    pub call: Call,
}

impl Request {
    pub fn new(id: RequestId, call: Call) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            call,
        }
    }
}

/// Success or failure, in JSON-RPC's mutually exclusive `result`/`error` shape.
///
/// An enum and not two `Option` fields: the two-`Option` shape can hold both at once and can hold
/// neither, and both of those are frames a client cannot act on.
///
/// The success body is JSON rather than a typed result — see [`crate::method`] for why: JSON-RPC
/// does not put the method on a response, so typing it here would require the envelope to carry a
/// field that is not on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    #[serde(rename = "result")]
    Result(serde_json::Value),
    #[serde(rename = "error")]
    Error(RpcError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl Response {
    pub fn ok(id: RequestId, result: &crate::method::MethodResult) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            outcome: Outcome::Result(result.to_body()),
        }
    }

    pub fn err(id: RequestId, error: RpcError) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            outcome: Outcome::Error(error),
        }
    }
}

/// A supervisor→client notification: a request with no `id`, and therefore no answer. See
/// [`crate::notify`] for why the supervisor must never be able to wait on a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    #[serde(flatten)]
    pub event: Event,
}

impl Notification {
    pub fn new(event: Event) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            event,
        }
    }
}

/// Anything that can appear on one NDJSON line.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Frame {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

impl Frame {
    /// One frame, one `\n`-terminated line.
    ///
    /// `serde_json::to_string` never emits a raw newline — it escapes them inside strings — so the
    /// only `\n` in the output is the terminator. That is a property the reader depends on
    /// absolutely, and it is asserted by test rather than trusted, because the day a field is
    /// serialized with a pretty-printer is the day every reader on the socket desynchronizes.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self)
            .expect("a frame is plain data and cannot fail to serialize");
        s.push('\n');
        s
    }

    /// Parse one line. The trailing newline is optional so a caller that already split on `\n`
    /// need not re-append one.
    ///
    /// Classification is by key presence rather than by trial deserialization — see the module
    /// doc. Each of the four `(method, id)` combinations gets its own sentence.
    pub fn from_line(line: &str) -> Result<Frame, RpcError> {
        let value: serde_json::Value = serde_json::from_str(line.trim_end_matches('\n'))
            .map_err(|e| RpcError::parse(format!("not JSON: {e}")))?;
        let obj = value.as_object().ok_or_else(|| {
            RpcError::invalid_request("a JSON-RPC frame is a JSON object; this line is not")
        })?;

        let method = obj.get("method").map(|m| {
            m.as_str().map(str::to_owned).ok_or_else(|| {
                RpcError::invalid_request("`method` must be a string naming a method")
            })
        });
        // Present-and-null is a different case from absent, and only the first is an error: a
        // notification legitimately has no `id` at all.
        let id_is_null = obj.get("id").is_some_and(serde_json::Value::is_null);
        let has_id = obj.contains_key("id") && !id_is_null;
        if id_is_null {
            return Err(RpcError::invalid_request(
                "a null `id` cannot be correlated with a request; marion refuses it rather than \
                 discard an answer a caller is still waiting for",
            ));
        }

        match (method, has_id) {
            (Some(method), true) => {
                let method = method?;
                if crate::Method::from_wire(&method).is_none() {
                    return Err(RpcError::method_not_found(format!(
                        "no such method: {method}"
                    )));
                }
                serde_json::from_value::<Request>(value)
                    .map(Frame::Request)
                    .map_err(|e| RpcError::invalid_params(format!("{method}: {e}")))
            }
            (Some(method), false) => {
                let method = method?;
                if !Event::METHODS.contains(&method.as_str()) {
                    return Err(RpcError::method_not_found(format!(
                        "no such notification: {method}"
                    )));
                }
                serde_json::from_value::<Notification>(value)
                    .map(Frame::Notification)
                    .map_err(|e| RpcError::invalid_params(format!("{method}: {e}")))
            }
            (None, true) => serde_json::from_value::<Response>(value)
                .map(Frame::Response)
                .map_err(|e| RpcError::invalid_request(format!("not a valid response: {e}"))),
            (None, false) => Err(RpcError::invalid_request(
                "a frame carries a `method` or an `id`; this one carries neither, so it is neither \
                 a call, an answer, nor an event",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::FailureKind;
    use crate::method::{Method, MethodResult};
    use crate::model::*;
    use crate::params::*;
    use crate::result::NodeCancelResult;
    use marion_core::contract::{AgentId, ExitStatus};
    use marion_core::encoding::SystemTime;
    use marion_core::node::{NodeState, ReapState};

    fn req() -> Request {
        Request::new(
            RequestId::Number(1),
            Call::NodePrompt(NodePromptParams {
                agent_id: AgentId("a".into()),
                text: "go".into(),
            }),
        )
    }

    fn resp() -> Response {
        Response::ok(
            RequestId::Number(1),
            &MethodResult::NodeCancel(NodeCancelResult {
                state: NodeState::Exited(ExitStatus::Cancelled),
            }),
        )
    }

    fn note() -> Notification {
        Notification::new(Event::NodeState {
            agent_id: AgentId("a".into()),
            state: NodeState::Idle,
            reap_state: ReapState::Live,
            ts: SystemTime::from_unix_millis(0),
        })
    }

    fn every_frame() -> Vec<Frame> {
        vec![
            Frame::Request(req()),
            Frame::Response(resp()),
            Frame::Response(Response::err(
                RequestId::Text("abc".into()),
                RpcError::refused("a", "the node is running; use node/steer", "§6.3"),
            )),
            Frame::Notification(note()),
        ]
    }

    #[test]
    fn every_frame_round_trips_through_a_line() {
        for f in every_frame() {
            let line = f.to_line();
            assert_eq!(Frame::from_line(&line).unwrap(), f, "{line}");
            // And without the terminator, since a caller that split on '\n' has already removed it.
            assert_eq!(Frame::from_line(line.trim_end()).unwrap(), f);
        }
    }

    #[test]
    fn a_line_contains_exactly_one_newline_and_it_is_last() {
        // The property every NDJSON reader depends on. A pretty-printed frame would desynchronize
        // the socket rather than fail visibly, which is why this is a test and not a comment.
        let f = Frame::Request(Request::new(
            RequestId::Text("a\nb".into()),
            Call::NodePrompt(NodePromptParams {
                agent_id: AgentId("a".into()),
                // A payload that itself contains newlines is the realistic case: a prompt is
                // multi-line more often than not.
                text: "line one\nline two\r\n".into(),
            }),
        ));
        let line = f.to_line();
        assert_eq!(line.matches('\n').count(), 1, "{line}");
        assert!(line.ends_with('\n'));
        assert_eq!(Frame::from_line(&line).unwrap(), f);
    }

    #[test]
    fn frames_pin_their_wire_shapes() {
        assert_eq!(
            Frame::Request(req()).to_line(),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"node/prompt\",\"params\":{\"agent_id\":\"a\",\"text\":\"go\"}}\n"
        );
        assert_eq!(
            Frame::Response(resp()).to_line(),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"state\":{\"Exited\":\"Cancelled\"}}}\n"
        );
        assert_eq!(
            Frame::Notification(note()).to_line(),
            "{\"jsonrpc\":\"2.0\",\"method\":\"node/state\",\"params\":{\"agent_id\":\"a\",\"state\":\"Idle\",\"reap_state\":\"Live\",\"ts\":\"1970-01-01T00:00:00.000Z\"}}\n"
        );
        assert_eq!(
            Frame::Response(Response::err(
                RequestId::Number(2),
                RpcError::internal("lock poisoned")
            ))
            .to_line(),
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32603,\"message\":\"lock poisoned\",\"data\":{\"kind\":\"Internal\",\"spec\":\"\"}}}\n"
        );
    }

    #[test]
    fn a_request_id_may_be_a_string_or_a_number_and_comes_back_as_it_went() {
        for id in [RequestId::Number(-7), RequestId::Text("req-1".into())] {
            let f = Frame::Response(Response::ok(
                id.clone(),
                &MethodResult::NodeCancel(NodeCancelResult {
                    state: NodeState::Idle,
                }),
            ));
            let Frame::Response(r) = Frame::from_line(&f.to_line()).unwrap() else {
                panic!("classified wrong")
            };
            assert_eq!(r.id, id);
        }
    }

    #[test]
    fn a_null_id_is_refused_by_name() {
        let e = Frame::from_line(r#"{"jsonrpc":"2.0","id":null,"result":{}}"#).unwrap_err();
        assert_eq!(e.code, crate::error::INVALID_REQUEST);
        assert!(e.message.contains("null `id`"), "{e}");
    }

    #[test]
    fn a_wrong_protocol_version_is_refused_rather_than_ignored() {
        let e = Frame::from_line(
            r#"{"jsonrpc":"1.0","id":1,"method":"node/get","params":{"agent_id":"a"}}"#,
        )
        .unwrap_err();
        assert!(e.message.contains("jsonrpc must be"), "{e}");
    }

    #[test]
    fn each_malformed_shape_gets_its_own_sentence() {
        let cases: [(&str, &str); 5] = [
            ("not json at all", "not JSON"),
            ("[1,2,3]", "is a JSON object"),
            (r#"{"jsonrpc":"2.0"}"#, "carries neither"),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"node/frobnicate","params":{}}"#,
                "no such method: node/frobnicate",
            ),
            (
                r#"{"jsonrpc":"2.0","method":"node/frobnicated","params":{}}"#,
                "no such notification: node/frobnicated",
            ),
        ];
        for (line, want) in cases {
            let e = Frame::from_line(line).unwrap_err();
            assert!(e.message.contains(want), "for {line}: got {e}");
            // None of these is a marion refusal — they are framing failures, and claiming a
            // FailureKind for them would invent a classification.
            assert_eq!(e.kind(), None);
        }
    }

    #[test]
    fn an_unknown_parameter_reaches_the_caller_naming_the_method_and_the_field() {
        let e = Frame::from_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{"agent_type":"t","prompt":"p","background":true}}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, crate::error::INVALID_PARAMS);
        assert!(e.message.contains("agent/spawn"), "{e}");
        assert!(e.message.contains("background"), "{e}");
    }

    #[test]
    fn a_response_carries_a_result_or_an_error_and_never_both() {
        // The two-Option shape could hold both; this one cannot, and the wire proves it.
        let both = r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#;
        // serde's externally-tagged enum takes the first matching key; what matters is that the
        // parsed value is one of them, never a struct holding two.
        let Frame::Response(r) = Frame::from_line(both).unwrap() else {
            panic!("classified wrong")
        };
        assert!(matches!(r.outcome, Outcome::Result(_) | Outcome::Error(_)));
    }

    #[test]
    fn an_error_frame_survives_the_round_trip_with_its_classification() {
        // The point of the error design: a refusal must still be a refusal after a socket.
        let f = Frame::Response(Response::err(
            RequestId::Number(3),
            RpcError::unsupported("a", "this harness cannot steer", "§3.3"),
        ));
        let Frame::Response(r) = Frame::from_line(&f.to_line()).unwrap() else {
            panic!("classified wrong")
        };
        let Outcome::Error(e) = r.outcome else {
            panic!("lost the error")
        };
        assert_eq!(e.kind(), Some(FailureKind::Unsupported));
        assert!(e.is_refusal());
    }

    #[test]
    fn a_policy_set_request_is_refused_on_the_wire_with_its_reason() {
        // End to end: the uninhabited params type reaching a caller as a sentence rather than as a
        // dropped field.
        let e = Frame::from_line(r#"{"jsonrpc":"2.0","id":1,"method":"policy/set","params":{}}"#)
            .unwrap_err();
        assert_eq!(e.code, crate::error::INVALID_PARAMS);
        assert!(e.message.contains("policy/set names no policy"), "{e}");
    }

    #[test]
    fn a_session_quit_frame_carries_its_disposition_and_nothing_defaults() {
        // §7.3.1's distinction, on the wire: the disposition is a required field, so a quit frame
        // that omits it is a parse error rather than a quit with a guessed intent.
        let e = Frame::from_line(r#"{"jsonrpc":"2.0","id":1,"method":"session/quit","params":{}}"#)
            .unwrap_err();
        assert!(e.message.contains("disposition"), "{e}");

        let f = Frame::Request(Request::new(
            RequestId::Number(9),
            Call::SessionQuit(SessionQuitParams {
                disposition: QuitDisposition::KillTree {
                    confirmed: vec![AgentId("a".into())],
                },
            }),
        ));
        assert_eq!(Frame::from_line(&f.to_line()).unwrap(), f);
    }

    #[test]
    fn the_full_request_response_cycle_types_a_result_by_its_pending_method() {
        // How a client actually uses this crate: send a call, keep (id -> method), decode the
        // answer with it. The seam the envelope deliberately leaves untyped.
        let call = Call::NodeCancel(NodeCancelParams {
            agent_id: AgentId("a".into()),
        });
        let pending = (RequestId::Number(1), call.method());
        let sent = Frame::Request(Request::new(pending.0.clone(), call)).to_line();
        assert!(sent.contains(r#""method":"node/cancel""#));

        let answer = Frame::Response(Response::ok(
            pending.0.clone(),
            &MethodResult::NodeCancel(NodeCancelResult {
                state: NodeState::Exited(ExitStatus::Cancelled),
            }),
        ))
        .to_line();
        let Frame::Response(r) = Frame::from_line(&answer).unwrap() else {
            panic!("classified wrong")
        };
        assert_eq!(r.id, pending.0);
        let Outcome::Result(body) = r.outcome else {
            panic!("expected a result")
        };
        assert_eq!(
            pending.1.decode_result(&body).unwrap(),
            MethodResult::NodeCancel(NodeCancelResult {
                state: NodeState::Exited(ExitStatus::Cancelled)
            })
        );
        assert_eq!(pending.1, Method::NodeCancel);
    }
}
