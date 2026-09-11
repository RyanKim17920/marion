//! `marion_core::proto` — the client↔supervisor JSON-RPC vocabulary (design §10).
//!
//! Formerly the `marion-proto` crate; merged here 2026-09-10, module tree and every public item
//! path intact. It belongs beside the model rather than beside it in a crate of its own because it
//! *spells* that model: [`NodeSummary`] is a projection of [`crate::node::NodeState`],
//! [`ReplayPoint`] of [`crate::ir::SrcSeq`]. See design §10's dated correction.
//!
//! **Types only.** No socket, no transport, no server, no client. The transport is a separate
//! change that depends on this one, and it is a small change precisely because the vocabulary and
//! its round-trip tests exist first: every question a socket would otherwise settle in passing —
//! how a refusal differs from a malfunction, what an unknown field means, what a `session/quit`
//! that killed nothing is allowed to say — is settled here, in types, with tests.
//!
//! # Which surface this is
//!
//! marion has **two** RPC surfaces and confusing them is the easiest mistake in this file:
//!
//! | | `marion_core::proto` | §5.4 |
//! |---|---|---|
//! | who calls | the **client** (the TUI, §5.6) over the §2 unix socket | an **agent**, over the per-child MCP bridge |
//! | transport | NDJSON JSON-RPC 2.0 | MCP tool calls |
//! | verbs | the fifteen below | `status`, `list`, `wait`, `send`, `cancel`, `report`, `spawn` |
//! | authorization | none per-verb: the client is the operator | per verb, per target, per target state, against a per-node capability token bound to an `AgentId` |
//!
//! §5.4's per-verb table is **not** this module's authorization model and must not be copied into
//! it. The clearest case is `Blocked(Descendants)`: §5.4 denies an *agent's* `send` against such a
//! node outright, because that hold belongs to marion — but §7.6's hold is *resolved by a
//! re-prompt*, and §6.3 names exactly two callers of that path, the user via `node/prompt` and
//! marion itself. Importing §5.4's denial here would make the one verb that can release the hold
//! unable to reach the held node. See [`Delivery::for_state`].
//!
//! # The method list is fifteen
//!
//! §10's prose says fourteen; it is stale. §2 (lines 87–89) lists fifteen and §2 line 91 documents
//! `session/quit` immediately below the list as the method that was added. Fifteen is the count,
//! and [`Method::ALL`] is pinned to it by test.
//!
//! # Framing
//!
//! NDJSON: one JSON-RPC 2.0 message per `\n`-terminated line, no embedded newlines. See
//! [`Frame::to_line`] and [`Frame::from_line`]; the no-embedded-newline property is a test, not a
//! comment, because it is the only thing standing between a multi-line message body and a reader
//! that splits on `\n`.
//!
//! # Unknown fields are rejected in params, tolerated elsewhere
//!
//! Every params struct carries `#[serde(deny_unknown_fields)]`. Results, notification payloads and
//! the envelope do not. The asymmetry is the point, and both halves have precedent in this repo:
//!
//! * **Params are strict** for the reason §3.1 gives for agent-type frontmatter — *"unknown keys
//!   are a load error, not silently ignored"* — and for the reason §11 item 23 (commit `77557e3`)
//!   gives for spawn parameters: a caller that asks for something, gets no error, and is told
//!   nothing has been served a wrong answer. An ignored parameter is indistinguishable from an
//!   honoured one from the caller's side, which is the accept-and-ignore shape §12 keeps recording
//!   in other harnesses. A rejected one is a sentence the caller can read.
//! * **Results and notifications are tolerant** for the reason [`crate::ir::Provenance`]
//!   gives for its `#[serde(default)]`s: these types will gain fields, and a client that refuses to
//!   parse a supervisor one version newer than itself has converted an additive change into an
//!   outage. The directions are not symmetric because the risks are not: an ignored *parameter*
//!   silently changes what runs, an ignored *result field* only narrows what is displayed.
//!
//! # Doc register
//!
//! Comments here argue and cite. A comment that only restates the field name has been deleted.

pub mod envelope;
pub mod error;
pub mod input;
pub mod method;
pub mod model;
pub mod native;
pub mod notify;
pub mod pane;
pub mod params;
pub mod result;

pub use envelope::{
    ClientNotification, Frame, JsonRpcVersion, Notification, Outcome, Request, RequestId, Response,
};
pub use error::{ErrorData, FailureKind, RpcError};
pub use input::{Input, NodePaneReadyV1, NodePaneWriteV1};
pub use method::{Call, Method, MethodResult};
pub use model::{
    AttachMode, ClientGone, Delivery, DetachGuidance, ElicitationRequestId, ElicitationResponse,
    HarnessReport, KilledNode, NodeSummary, PermissionDecision, PermissionRequestId, ProbeMode,
    QuitDisposition, QuitOutcome, ReplayPoint, ReplyOutcome, ResidentReason, SupervisorDisposition,
};
pub use native::{
    NativeEnvVarV1, NativeLaunchContext, NativeLaunchContextV1, NativeLaunchContextV2,
    NativeOsValueConversionError, OpaqueOsValueV1, TerminalGeometryV1,
};
pub use notify::Event;
pub use pane::{OpaquePaneBytesV1, PaneFrameKindV1, PaneFrameV1, PaneReadyTokenV1};
/// Re-exported beside the models because it is one: `agent/spawn` is the only method whose caller
/// is not always a client, and every consumer of that distinction — the supervisor's handler, the
/// bridge, a test client — reaches for this type rather than for the params struct around it.
pub use params::SpawnCaller;
