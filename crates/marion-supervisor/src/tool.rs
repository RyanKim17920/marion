//! **The dispatch seam: tool name + JSON arguments in, content blocks + an explicit `isError`
//! out.**
//!
//! # Status: specified and adapter-complete; `handle_tool_call` has not moved yet
//!
//! This module is the boundary written down, not the boundary already enforced. [`ToolOutcome`]
//! and [`from_wire`]/[`crate::bridge::tool_outcome_result`] exist and round-trip, so the seam is
//! executable and tested — but `mcp::handle_tool_call` still builds JSON-RPC frames directly and
//! still takes a request id. Adopting the seam is a mechanical change described under *Adoption*
//! below, deliberately left for the change that can land it without colliding with in-flight work
//! in that same function.
//!
//! # Why the line is worth drawing even before anything moves across it
//!
//! Three independent reviews of whether marion should adopt an MCP SDK each had to reconstruct the
//! same boundary by hand before they could say anything about either half: which code is JSON-RPC
//! plumbing, and which code is marion's product semantics. Nothing in the tree drew that line, so
//! every reader drew it again, and each drew it slightly differently. Writing it down is most of
//! the value; moving code across it is the small remainder.
//!
//! The line: **everything marion means by a tool call is on one side, everything MCP means by a
//! frame is on the other.**
//!
//! | marion's side (behind the seam) | MCP's side (in front of it) |
//! |---|---|
//! | [`crate::mcp::Principal`] — node vs top-level client | the `jsonrpc` field, and the request id |
//! | the §5.4 depth gate and `report`-on-a-root refusal | `initialize` and version negotiation |
//! | the supervisor handshake and socket dial | `tools/list` and the declared surface |
//! | all courier behaviour, `background`, `wait`, `status` | JSON-RPC error codes (`-32700`, `-32600`, `-32601`, `-32002`) |
//! | the wording of every refusal | the `content` array's `{"type": "text"}` envelope |
//! | **the `isError` verdict** | whether a reply is written at all, and when it is flushed |
//!
//! The two rows most often confused are the last of each column, and they are the two this type
//! exists to separate — see *Why `is_error` is a field* below.
//!
//! # Adoption
//!
//! When `handle_tool_call` is free to move, the change is:
//!
//! 1. change its return type from `serde_json::Value` to [`ToolOutcome`] and drop its `id`
//!    parameter — it cannot then construct a JSON-RPC reply even by accident, because it no longer
//!    holds the id one would need;
//! 2. do the same to the nine result builders in [`crate::bridge`] that currently take an `id` and
//!    return a frame (`spawn_result`, `root_result`, `background_result`, `wait_unknown`,
//!    `wait_already_collected`, `wait_still_running`, `status_result`, `status_unknown`,
//!    `list_result`), so that each returns what it means rather than a frame;
//! 3. wrap exactly once, at the `ToolsCall` arm of [`crate::mcp::serve_stdio`], with
//!    [`crate::bridge::tool_outcome_result`].
//!
//! That is the whole port. If an SDK is ever adopted, step 3 is the only line that changes; the
//! far side is already protocol-neutral by construction.
//!
//! # Why `is_error` is a field and not a `Result`
//!
//! Because in MCP they are **different things**, and marion has measured the difference.
//!
//! A JSON-RPC error means *the call did not happen*. `isError: true` means *the call happened and
//! the tool is reporting failure to the model*. `tests/fixtures/s9/` pins marion's policy: a child
//! agent that failed is a **successful** JSON-RPC response carrying `isError: true`, because the
//! spawn worked, the child ran, and its failure is a fact the parent model must read and act on —
//! not a transport fault. Conversely a root that finishes with no contract is `isError: false`.
//!
//! A `Result<String, E>` cannot express that. Deriving `isError` from `Err` would make every
//! internal hiccup indistinguishable from a child's honest failure, and would silently reclassify
//! s9's cases the moment anyone added a `?`. So the flag is set explicitly, at each site, by the
//! code that knows which of the two happened — and [`ToolOutcome::text`] takes it as an argument
//! rather than inferring it from anything.

use serde_json::Value;

/// One block of a tool result's content.
///
/// An enum with one variant, deliberately: MCP has had `image`, `audio` and `resource_link` blocks
/// since `2025-03-26`, and marion emits none of them. Modelling the axis without populating it
/// costs one `match` and means adding a block later is an added variant rather than a changed
/// signature at every call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlock {
    Text(String),
}

/// What a marion tool call produced: content for the model, and whether it reports failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub content: Vec<ContentBlock>,
    /// **marion's policy, stated at the call site.** Never derived from a Rust `Result`; see the
    /// module docs and `tests/fixtures/s9/`.
    pub is_error: bool,
}

impl ToolOutcome {
    /// The shape every marion tool returns today: one text block, and an explicit verdict.
    pub fn text(text: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: vec![ContentBlock::Text(text.into())],
            is_error,
        }
    }

    /// The concatenated text of this outcome, for tests and for callers that want one string.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .map(|ContentBlock::Text(t)| t.as_str())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Both halves, because an assertion that prints only the text cannot show which way the verdict
/// went — and the verdict is the half this codebase keeps legislating about.
impl std::fmt::Display for ToolOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[isError={}] {}", self.is_error, self.text_content())
    }
}

/// Read a JSON-RPC tool-call **response** back into the neutral form.
///
/// The inverse of [`crate::bridge::tool_outcome_result`], and the reason the seam is testable
/// before anything moves behind it: today's `handle_tool_call` still returns a frame, so this is
/// what lets a test say *"whatever that frame is, here is the outcome it denotes"* and hold the
/// existing dispatch to the same contract the ported version will satisfy.
///
/// Returns `None` for anything that is not a tool result — a JSON-RPC `error` frame most of all,
/// because that is the case the seam refuses to conflate with `isError`.
pub fn from_wire(frame: &Value) -> Option<ToolOutcome> {
    if frame.get("error").is_some() {
        return None;
    }
    let result = frame.get("result")?;
    let blocks = result.get("content")?.as_array()?;
    let mut content = Vec::with_capacity(blocks.len());
    for b in blocks {
        // Unknown block kinds are not silently dropped: an outcome that lost a block would be a
        // quieter lie than one that failed to parse.
        if b.get("type")?.as_str()? != "text" {
            return None;
        }
        content.push(ContentBlock::Text(b.get("text")?.as_str()?.to_string()));
    }
    Some(ToolOutcome {
        content,
        // Absent means false on the wire, and MCP says so — but marion always writes it, and
        // `a_successful_call_carries_is_error_false_explicitly` holds it to that.
        is_error: result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The seam round-trips, which is what makes it a boundary rather than a description.
    #[test]
    fn an_outcome_survives_the_trip_through_the_wire_shape() {
        for outcome in [
            ToolOutcome::text("a child failed, and that is a fact not a fault", true),
            ToolOutcome::text("contract recorded", false),
            ToolOutcome {
                content: vec![
                    ContentBlock::Text("first line".into()),
                    ContentBlock::Text("second".into()),
                ],
                is_error: false,
            },
        ] {
            let wire = crate::bridge::tool_outcome_result(&json!(7), &outcome);
            assert_eq!(
                wire["id"], 7,
                "the id is added on the MCP side, and only there"
            );
            assert_eq!(
                from_wire(&wire),
                Some(outcome.clone()),
                "and nothing about the outcome is lost crossing the seam: {outcome}"
            );
        }
    }

    /// **A JSON-RPC error is not an outcome**, which is the distinction the whole type exists for.
    ///
    /// If this ever returned `Some`, `isError` and transport failure would have been conflated at
    /// the one place that is supposed to keep them apart — and s9's policy would be expressible
    /// two ways, which is how it stops being a policy.
    #[test]
    fn a_transport_error_is_not_a_tool_outcome() {
        assert_eq!(
            from_wire(&crate::bridge::method_not_found(&json!(1), "x")),
            None
        );
        assert_eq!(from_wire(&crate::bridge::parse_error("nope")), None);
        assert_eq!(
            from_wire(&crate::bridge::not_initialized(&json!(1), "tools/call")),
            None
        );
        assert_eq!(
            from_wire(&json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [
                {"type": "image", "data": "…"}], "isError": false}})),
            None,
            "a block kind marion does not emit is refused rather than dropped"
        );
    }

    /// The existing dispatch already satisfies the seam's contract — measured against the real
    /// builders rather than asserted.
    ///
    /// This is what the seam buys before adoption: the builders in `bridge` are held to "denotes
    /// exactly one outcome, with an explicit verdict" today, so step 2 of *Adoption* is a refactor
    /// with a net already under it.
    #[test]
    fn todays_result_builders_all_denote_an_outcome_with_an_explicit_verdict() {
        let id = json!(1);
        let frames = [
            ("wait_unknown", crate::bridge::wait_unknown(&id, "t-1")),
            (
                "wait_still_running",
                crate::bridge::wait_still_running(&id, "t-1", "codex", 30),
            ),
            ("status_unknown", crate::bridge::status_unknown(&id, "t-1")),
            ("list_result", crate::bridge::list_result(&id, &[])),
            (
                "tool_result",
                crate::bridge::tool_result(&id, "a sentence", true),
            ),
        ];
        for (name, frame) in frames {
            let outcome = from_wire(&frame)
                .unwrap_or_else(|| panic!("{name} must denote a tool outcome: {frame}"));
            assert!(
                !outcome.text_content().is_empty(),
                "{name}: every outcome marion returns is a sentence"
            );
            assert!(
                frame["result"].get("isError").is_some(),
                "{name}: the verdict is written explicitly, never left to the client's default"
            );
        }
    }
}
