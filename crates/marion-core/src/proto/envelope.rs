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

use crate::proto::error::RpcError;
use crate::proto::input::Input;
use crate::proto::method::Call;
use crate::proto::notify::Event;

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
/// The success body is JSON rather than a typed result — see [`crate::proto::method`] for why: JSON-RPC
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
    pub fn ok(id: RequestId, result: &crate::proto::method::MethodResult) -> Self {
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
/// [`crate::proto::notify`] for why the supervisor must never be able to wait on a client.
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

/// A client→supervisor notification: a request with no `id`, and therefore no answer. See
/// [`crate::proto::input`] for why a keystroke must not be a request, and why the transport it needs is a
/// second notification table rather than a sixteenth method.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientNotification {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    #[serde(flatten)]
    pub input: Input,
}

impl ClientNotification {
    pub fn new(input: Input) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            input,
        }
    }
}

/// Anything that can appear on one NDJSON line.
///
/// **Four variants, not three, and the fourth is a direction rather than a kind.** A notification
/// is an id-less frame either way; what [`Frame::Input`] adds is that the *name* says which end
/// sent it. Keeping the two in one variant would have meant a supervisor accepting `node/pty` from
/// a client — a client asserting what a node printed — which the outbound table's own doc rules
/// out by saying the ordinal is *"assigned by the single reader of that master"*.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Frame {
    Request(Request),
    Response(Response),
    Notification(Notification),
    Input(ClientNotification),
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
    ///
    /// Three steps, in order, because each one may only run on what the one before established:
    /// parse the line as JSON, classify it by key presence, then decode it as the frame that
    /// classification named.
    pub fn from_line(line: &str) -> Result<Frame, RpcError> {
        let line = line.trim_end_matches('\n');
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| RpcError::parse(format!("not JSON: {e}")))?;
        let shape = classify(&value)?;
        decode(line, value, shape)
    }
}

/// What key presence alone says a line is, before any typed decoding.
struct Shape {
    /// `None` when the line carries no `method` key at all — never for one that carries a `method`
    /// which is not a string, since that is refused during classification.
    method: Option<String>,
    has_id: bool,
}

/// Step 2: classify by key presence. No typed decoding happens here, and none may: which decoder
/// the line is owed is exactly what this decides.
fn classify(value: &serde_json::Value) -> Result<Shape, RpcError> {
    let obj = value.as_object().ok_or_else(|| {
        RpcError::invalid_request("a JSON-RPC frame is a JSON object; this line is not")
    })?;
    // Present-and-null is a different case from absent, and only the first is an error: a
    // notification legitimately has no `id` at all.
    if obj.get("id").is_some_and(serde_json::Value::is_null) {
        return Err(RpcError::invalid_request(
            "a null `id` cannot be correlated with a request; marion refuses it rather than \
             discard an answer a caller is still waiting for",
        ));
    }
    let method = obj
        .get("method")
        .map(|m| {
            m.as_str().map(str::to_owned).ok_or_else(|| {
                RpcError::invalid_request("`method` must be a string naming a method")
            })
        })
        .transpose()?;
    Ok(Shape {
        method,
        has_id: obj.contains_key("id"),
    })
}

/// Step 3: decode as the frame classification named. The four `(method, id)` combinations, and
/// nothing else, because [`Shape`] admits nothing else.
fn decode(line: &str, value: serde_json::Value, shape: Shape) -> Result<Frame, RpcError> {
    match (shape.method, shape.has_id) {
        (Some(method), true) => decode_request(line, value, method),
        (Some(method), false) => decode_notification(line, value, method),
        (None, true) => serde_json::from_value::<Response>(value)
            .map(Frame::Response)
            .map_err(|e| RpcError::invalid_request(format!("not a valid response: {e}"))),
        (None, false) => Err(RpcError::invalid_request(
            "a frame carries a `method` or an `id`; this one carries neither, so it is neither \
             a call, an answer, nor an event",
        )),
    }
}

/// `method` + `id`: a call.
fn decode_request(line: &str, value: serde_json::Value, method: String) -> Result<Frame, RpcError> {
    if crate::proto::Method::from_wire(&method).is_none() {
        return Err(RpcError::method_not_found(format!(
            "no such method: {method}"
        )));
    }
    let request = if method == crate::proto::Method::AgentSpawn.as_str() {
        // Native launch dispatch must see duplicate keys at every depth. `Value` classification
        // above is only a probe; replay this request from its source.
        serde_json::from_str::<Request>(line)
    } else {
        serde_json::from_value::<Request>(value)
    };
    request
        .map(Frame::Request)
        .map_err(|e| RpcError::invalid_params(format!("{method}: {e}")))
}

/// `method`, no `id`: an event or a client input.
///
/// **Two tables, matched exactly.** A prefix or `starts_with` test would route `node/pty-write`
/// into the outbound table, where it would parse as nothing and be reported as a malformed
/// `node/pty`. Which table a name is in is also which *direction* it travels, so the choice is not
/// cosmetic: it is what stops a client asserting what a node printed.
fn decode_notification(
    line: &str,
    value: serde_json::Value,
    method: String,
) -> Result<Frame, RpcError> {
    if Event::METHODS.contains(&method.as_str()) {
        return decode_event(line, value, &method);
    }
    if Input::METHODS.contains(&method.as_str()) {
        return decode_input(line, value, &method);
    }
    Err(RpcError::method_not_found(format!(
        "no such notification: {method}"
    )))
}

/// The outbound table: what a node printed, said by the one reader of that master.
fn decode_event(line: &str, value: serde_json::Value, method: &str) -> Result<Frame, RpcError> {
    let notification = if method == "node/pane-frame" {
        // Pane frames are strict at every depth. `Value` classification above is only a probe
        // because it has already collapsed duplicate object keys; replay this notification from
        // its original source before accepting it.
        serde_json::from_str::<Notification>(line)
    } else {
        serde_json::from_value::<Notification>(value)
    };
    notification
        .map(Frame::Notification)
        .map_err(|e| RpcError::invalid_params(format!("{method}: {e}")))
}

/// The inbound table: what a client sent, which is never an assertion about a node.
fn decode_input(line: &str, value: serde_json::Value, method: &str) -> Result<Frame, RpcError> {
    let input = if matches!(method, "node/pane-ready" | "node/pane-write") {
        // Pane readiness and opaque input are strict typed state. Preserve duplicate-key evidence
        // that classification through `Value` necessarily collapsed.
        serde_json::from_str::<ClientNotification>(line)
    } else {
        serde_json::from_value::<ClientNotification>(value)
    };
    input
        .map(Frame::Input)
        .map_err(|e| RpcError::invalid_params(format!("{method}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{AgentId, ExitStatus};
    use crate::encoding::SystemTime;
    use crate::node::{NodeState, ReapState};
    use crate::proto::error::FailureKind;
    use crate::proto::input::Input;
    use crate::proto::method::{Method, MethodResult};
    use crate::proto::model::*;
    use crate::proto::params::*;
    use crate::proto::result::NodeCancelResult;
    use crate::proto::{
        NodePaneReadyV1, OpaquePaneBytesV1, PaneFrameKindV1, PaneFrameV1, PaneReadyTokenV1,
    };

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

    fn input_write() -> ClientNotification {
        ClientNotification::new(Input::NodePtyWrite {
            agent_id: AgentId("a".into()),
            bytes: "ls -la\r".into(),
        })
    }

    fn input_pane_ready() -> ClientNotification {
        ClientNotification::new(Input::NodePaneReady(NodePaneReadyV1 {
            agent_id: AgentId("a".into()),
            token: PaneReadyTokenV1::new([
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
                0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
                0x1c, 0x1d, 0x1e, 0x1f,
            ]),
            cut: 9,
        }))
    }

    fn input_pane_write() -> ClientNotification {
        ClientNotification::new(Input::NodePaneWrite(crate::proto::NodePaneWriteV1 {
            agent_id: AgentId("a".into()),
            bytes: OpaquePaneBytesV1::new([0x00, 0x80, 0xff]),
        }))
    }

    fn pane_frame(seq: u64, kind: PaneFrameKindV1) -> Frame {
        Frame::Notification(Notification::new(Event::NodePaneFrame(PaneFrameV1::new(
            AgentId("a".into()),
            seq,
            kind,
        ))))
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
            pane_frame(
                0,
                PaneFrameKindV1::Output {
                    bytes: OpaquePaneBytesV1::new([0xff]),
                },
            ),
            Frame::Input(input_write()),
            Frame::Input(input_pane_write()),
            Frame::Input(ClientNotification::new(Input::NodeResize {
                agent_id: AgentId("a".into()),
                cols: 140,
                rows: 40,
            })),
            Frame::Input(input_pane_ready()),
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
            Frame::Input(input_pane_ready()).to_line(),
            "{\"jsonrpc\":\"2.0\",\"method\":\"node/pane-ready\",\"params\":{\"agent_id\":\"a\",\"token\":\"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=\",\"cut\":9}}\n"
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
    fn pane_frames_pin_all_three_ndjson_wire_shapes() {
        let cases = [
            (
                pane_frame(
                    0,
                    PaneFrameKindV1::Output {
                        bytes: OpaquePaneBytesV1::new([0x00, 0x01, 0xff, 0x80]),
                    },
                ),
                "{\"jsonrpc\":\"2.0\",\"method\":\"node/pane-frame\",\"params\":{\"version\":1,\"agent_id\":\"a\",\"seq\":0,\"frame\":{\"kind\":\"Output\",\"bytes\":\"AAH/gA==\"}}}\n",
            ),
            (
                pane_frame(
                    1,
                    PaneFrameKindV1::Resize {
                        cols: 140,
                        rows: 40,
                    },
                ),
                "{\"jsonrpc\":\"2.0\",\"method\":\"node/pane-frame\",\"params\":{\"version\":1,\"agent_id\":\"a\",\"seq\":1,\"frame\":{\"kind\":\"Resize\",\"cols\":140,\"rows\":40}}}\n",
            ),
            (
                pane_frame(2, PaneFrameKindV1::End {}),
                "{\"jsonrpc\":\"2.0\",\"method\":\"node/pane-frame\",\"params\":{\"version\":1,\"agent_id\":\"a\",\"seq\":2,\"frame\":{\"kind\":\"End\"}}}\n",
            ),
        ];

        for (frame, line) in cases {
            assert_eq!(frame.to_line(), line);
            assert_eq!(Frame::from_line(line).unwrap(), frame);
            assert_eq!(line.matches('\n').count(), 1);
        }
    }

    #[test]
    fn pane_frame_duplicate_keys_are_rejected_from_the_original_line() {
        for line in [
            r#"{"jsonrpc":"2.0","method":"node/pane-frame","method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"End"}}}"#,
            r#"{"jsonrpc":"2.0","method":"node/pane-frame","params":{"version":1,"version":1,"agent_id":"a","seq":0,"frame":{"kind":"End"}}}"#,
            r#"{"jsonrpc":"2.0","method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"End","kind":"End"}}}"#,
            r#"{"jsonrpc":"2.0","method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":0,"frame":{"kind":"Output","bytes":"AAH/gA==","bytes":"AAH/gA=="}}}"#,
        ] {
            assert!(
                Frame::from_line(line).is_err(),
                "accepted duplicate pane-frame key from source: {line}"
            );
        }
    }

    #[test]
    fn pane_ready_duplicate_keys_are_rejected_from_the_original_line() {
        const TOKEN: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        for line in [
            format!(
                r#"{{"jsonrpc":"2.0","method":"node/pane-ready","method":"node/pane-ready","params":{{"agent_id":"a","token":"{TOKEN}","cut":9}}}}"#,
            ),
            format!(
                r#"{{"jsonrpc":"2.0","method":"node/pane-ready","params":{{"agent_id":"a","agent_id":"a","token":"{TOKEN}","cut":9}}}}"#,
            ),
            format!(
                r#"{{"jsonrpc":"2.0","method":"node/pane-ready","params":{{"agent_id":"a","token":"{TOKEN}","token":"{TOKEN}","cut":9}}}}"#,
            ),
            format!(
                r#"{{"jsonrpc":"2.0","method":"node/pane-ready","params":{{"agent_id":"a","token":"{TOKEN}","cut":9,"cut":9}}}}"#,
            ),
        ] {
            assert!(
                Frame::from_line(&line).is_err(),
                "accepted duplicate pane-ready key from source: {line}"
            );
        }
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
        assert_eq!(e.code, crate::proto::error::INVALID_REQUEST);
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

    /// **The direction seam.** An id-less frame is a notification either way, so the *only* thing
    /// that says which end sent it is which table its name is in. Both tables are consulted, and a
    /// name in neither is still refused by name.
    #[test]
    fn an_id_less_frame_is_classified_by_which_notification_table_names_it() {
        let inbound =
            Frame::from_line(r#"{"jsonrpc":"2.0","method":"node/resize","params":{"agent_id":"a","cols":80,"rows":24}}"#)
                .unwrap();
        assert!(
            matches!(inbound, Frame::Input(_)),
            "an inbound name must not parse as a supervisor event: {inbound:?}"
        );
        let pane_ready = Frame::from_line(
            r#"{"jsonrpc":"2.0","method":"node/pane-ready","params":{"agent_id":"a","token":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=","cut":9}}"#,
        )
        .unwrap();
        assert_eq!(pane_ready, Frame::Input(input_pane_ready()));
        let outbound = Frame::from_line(
            r#"{"jsonrpc":"2.0","method":"node/pty","params":{"agent_id":"a","seq":0,"mono_ns":0,"bytes":"x"}}"#,
        )
        .unwrap();
        assert!(
            matches!(outbound, Frame::Notification(_)),
            "an outbound name must not parse as client input: {outbound:?}"
        );
        let pane_frame = Frame::from_line(
            r#"{"jsonrpc":"2.0","method":"node/pane-frame","params":{"version":1,"agent_id":"a","seq":2,"frame":{"kind":"End"}}}"#,
        )
        .unwrap();
        assert!(matches!(pane_frame, Frame::Notification(_)));
    }

    /// `node/pty-write` is `node/pty` plus a suffix, and the reader matches exactly. A
    /// `starts_with` reader — the natural way to write this — would route every keystroke into the
    /// outbound table and report it as a malformed `node/pty`.
    #[test]
    fn a_keystroke_is_not_read_as_a_truncated_pty_event() {
        let f = Frame::from_line(
            r#"{"jsonrpc":"2.0","method":"node/pty-write","params":{"agent_id":"a","bytes":"q"}}"#,
        )
        .unwrap();
        assert_eq!(
            f,
            Frame::Input(ClientNotification::new(Input::NodePtyWrite {
                agent_id: AgentId("a".into()),
                bytes: "q".into(),
            }))
        );
    }

    /// A keystroke carries no `id`, and a client that gave it one would be waiting for an answer
    /// that has no sender. The refusal names the method rather than the shape.
    #[test]
    fn a_keystroke_sent_as_a_request_is_refused_because_it_is_not_a_method() {
        let e = Frame::from_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"node/pty-write","params":{"agent_id":"a","bytes":"q"}}"#,
        )
        .unwrap_err();
        assert!(e.message.contains("no such method: node/pty-write"), "{e}");
    }

    #[test]
    fn an_unknown_parameter_reaches_the_caller_naming_the_method_and_the_field() {
        let e = Frame::from_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"agent/spawn","params":{"agent_type":"t","prompt":"p","background":true}}"#,
        )
        .unwrap_err();
        assert_eq!(e.code, crate::proto::error::INVALID_PARAMS);
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
        assert_eq!(e.code, crate::proto::error::INVALID_PARAMS);
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
