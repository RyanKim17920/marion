//! The canned provider (design §5.5).
//!
//! Replays scripted responses for four wire formats — Anthropic Messages for a Claude Code root,
//! OpenAI Responses for a Codex child, Gemini `generateContent` for a gemini child and OpenAI Chat
//! Completions for an opencode child — so a run costs nothing and repeats exactly. **Canning is not
//! translating:** each format replays its own recorded shape; nothing is converted between them,
//! down to the four different spellings the same `report` tool has on the four wires.
//!
//! # Dispatch on request *shape*, never on arrival order
//!
//! Claude Code 2.1.220 issues a **session-title generation request to the same base URL
//! concurrently with the first real turn** — measured 8 ms apart, so on a threaded provider the
//! order is a race. A positional script hands the scripted turn to the *title* request, the root
//! then emits plain text, `spawn` is never called, and nothing anywhere reports an error.
//!
//! The auxiliary request is identifiable by shape: `tools: []` plus an `output_config.format`
//! `json_schema` carrying a `{title}` property.

use serde_json::Value;

pub mod anthropic;
pub mod gate;
pub mod gemini;
pub mod openai;
pub mod reqlog;
pub mod responses;
pub mod script;
pub mod server;

pub use gate::{Answer, Hold, Rendezvous, TurnGate};
pub use script::{
    ChildStep, EditTurn, GeminiKind, GeminiStep, HANDLE_KEY, KeyRefusal, NodeScript, OpenAiStep,
    RootScript, RootStep, RootTurn, Script, ScriptedCall, Wire, wire_name,
};
pub use server::{CannedServer, Config};

/// What every canned completion says it spent, on each wire in that wire's own shape: 100 prompt
/// tokens of which 10 were cache reads, 20 completion tokens of which 5 were reasoning. Non-zero
/// and distinct per counter, so a test reading a node's recorded usage can tell each counter
/// arrived where it belongs and that a run of `n` completions recorded `n` times these — a zero
/// would pass either way.
pub const USAGE: CannedUsage = CannedUsage {
    input: 100,
    cached: 10,
    output: 20,
    reasoning: 5,
};

/// The counters one canned completion reports. See [`USAGE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CannedUsage {
    /// Prompt tokens, cache reads included, as the OpenAI wires count them.
    pub input: u64,
    pub cached: u64,
    /// Completion tokens, reasoning included.
    pub output: u64,
    pub reasoning: u64,
}

/// What a request is asking for, decided by shape alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// A real turn: carries a non-empty `tools` array.
    ScriptedTurn,
    /// Claude Code's concurrent session-title request. Answered with a fixed stub.
    SessionTitle,
}

/// Classify an Anthropic Messages request.
///
/// Deliberately *not* "is there a title schema" alone: a request with no tools is not a turn we
/// have a script for either way, and treating it as one is the failure this function exists to
/// prevent.
pub fn classify_anthropic(body: &Value) -> RequestKind {
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    if has_tools {
        RequestKind::ScriptedTurn
    } else {
        RequestKind::SessionTitle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_turn_is_recognised_by_its_tools() {
        let body = json!({"model": "claude", "tools": [{"name": "mcp__marion__spawn"}]});
        assert_eq!(classify_anthropic(&body), RequestKind::ScriptedTurn);
    }

    #[test]
    fn the_concurrent_title_request_is_not_mistaken_for_a_turn() {
        // Measured shape: empty tools, plus a json_schema output config with a title property.
        let body = json!({
            "model": "claude",
            "tools": [],
            "output_config": {"format": {"type": "json_schema", "schema": {
                "properties": {"title": {"type": "string"}}}}}
        });
        assert_eq!(
            classify_anthropic(&body),
            RequestKind::SessionTitle,
            "routing this to the scripted turn is what silently breaks the hop"
        );
    }

    #[test]
    fn a_missing_tools_key_is_not_a_turn() {
        assert_eq!(
            classify_anthropic(&json!({"model": "claude"})),
            RequestKind::SessionTitle
        );
    }
}
