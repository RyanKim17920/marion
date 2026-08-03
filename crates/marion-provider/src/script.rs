//! The scripts, and the *shape* predicates that choose between their steps.
//!
//! Every decision in this module is a function of the request body alone. That is not a stylistic
//! preference: see the module docs on [`crate`] — Claude Code issues a session-title request
//! concurrently with the first real turn, so "which request is this" is a race, and the Python
//! ancestor of this provider (`spikes/s6/canned_provider.py`) got it wrong by counting turns.
//! Nothing here counts anything.

use serde_json::{Value, json};

use crate::{RequestKind, anthropic, classify_anthropic, gemini, openai, responses};

/// `call_id` of the child's `apply_patch` step. Its later echo in `input` is how the provider
/// recognises that the patch step has already happened.
pub const PATCH_CALL_ID: &str = "call_marion_patch_1";
/// `call_id` of the child's `report` step, used the same way.
pub const REPORT_CALL_ID: &str = "call_marion_report_1";
/// `tool_use.id` of the root's single tool call. The root's `tool_result` quotes it back, and — as
/// S9 records — so does the `can_use_tool` frame's `tool_use_id`, which is how a permission ask is
/// tied to the call that provoked it.
pub const ROOT_TOOL_USE_ID: &str = "toolu_marion_spawn_1";

/// Which wire format a request is speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// Anthropic Messages, spoken by the Claude Code root.
    Anthropic,
    /// OpenAI Responses, spoken by the `codex exec` child.
    Responses,
    /// Gemini `generateContent`, spoken by a `gemini` child.
    ///
    /// `streaming` distinguishes `:streamGenerateContent?alt=sse` from `:generateContent`. It is
    /// the one thing on this wire that genuinely *is* a property of the path — the body is
    /// byte-identical either way — and it selects a framing, never a script step. Carrying it here
    /// keeps [`Script::respond`] a function of `(wire, body)` as it already was.
    Gemini {
        /// `true` for the SSE endpoint, `false` for the plain-JSON one.
        streaming: bool,
    },
    /// OpenAI Chat Completions, spoken by an `opencode` child.
    OpenAi,
}

/// The name this wire is recorded under in the request log.
pub fn wire_name(wire: Wire) -> &'static str {
    match wire {
        Wire::Anthropic => "anthropic",
        Wire::Responses => "responses",
        Wire::Gemini { .. } => "gemini",
        Wire::OpenAi => "openai",
    }
}

/// Tell the four wires apart.
///
/// The body is the primary evidence and the path only a tiebreak: every harness lets the operator
/// choose the base URL, so a path is configuration, whereas the top-level request key is the
/// protocol. Three of the four announce themselves outright — `contents` is Gemini, `input` is
/// Responses, `messages` is one of the two Chat wires — and the fourth needs one more step.
///
/// **The `messages` collision is real and is resolved on measured shape, not on the path.** Both
/// Anthropic Messages and OpenAI Chat Completions carry a top-level `messages` array, so the key
/// alone is not a discriminator; see [`messages_wire`] for which shapes inside it are.
pub fn classify_wire(path: &str, body: &Value) -> Option<Wire> {
    if body.get("contents").and_then(Value::as_array).is_some() {
        return Some(Wire::Gemini {
            streaming: gemini_streaming(path),
        });
    }
    if body.get("input").is_some() {
        return Some(Wire::Responses);
    }
    if body.get("messages").and_then(Value::as_array).is_some() {
        return Some(messages_wire(path, body));
    }
    if path.contains(":streamGenerateContent") || path.contains(":generateContent") {
        return Some(Wire::Gemini {
            streaming: gemini_streaming(path),
        });
    }
    if path.contains("/chat/completions") {
        return Some(Wire::OpenAi);
    }
    if path.contains("/messages") {
        return Some(Wire::Anthropic);
    }
    if path.contains("/responses") {
        return Some(Wire::Responses);
    }
    None
}

/// SSE or plain JSON, from the endpoint the CLI chose.
///
/// `:streamGenerateContent` and `alt=sse` travel together in every S12 capture; either alone is
/// taken as streaming, because answering SSE with JSON hangs the client and the reverse is an
/// obvious parse error.
fn gemini_streaming(path: &str) -> bool {
    path.contains(":streamGenerateContent") || path.contains("alt=sse")
}

/// Split the two wires that both call their transcript `messages`.
///
/// Measured discriminators, strongest first — each is a *structural* difference, present on every
/// request the respective harness makes, not a substring anywhere in the body:
///
/// 1. **The tool declarations.** Anthropic declares `tools[].input_schema` at the top of each tool
///    object; OpenAI wraps every tool as `{"type":"function","function":{…}}`. S13 saw the
///    `tools[].function` form on the wire from opencode, and the s1 Anthropic fixtures carry
///    `input_schema`. Both harnesses declare marion's MCP tools on *every* turn, so this is stable
///    rather than edge-triggered — the same property [`classify_root`] depends on.
/// 2. **The transcript.** Anthropic represents a call and its result as content *blocks*
///    (`{"type":"tool_use"}` / `{"type":"tool_result"}`) inside a message; OpenAI puts
///    `tool_calls` on an assistant message and answers with a whole message of `role: "tool"`.
///    These shapes cannot both be valid in one body.
///
/// Only if a body offers neither — a bare `messages` array, which neither harness ever sends —
/// does the path break the tie, and then in exactly the way the rest of this function uses it: as
/// configuration, consulted last. The tie defaults to Anthropic, which is what this provider
/// answered before the two Chat wires had to share the key.
///
/// Deliberately *not* used as discriminators: `max_tokens` (Anthropic requires it, but OpenAI
/// permits it, so its presence proves nothing), and `stream: true` (both wires stream).
fn messages_wire(path: &str, body: &Value) -> Wire {
    for tool in body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if tool.get("function").and_then(Value::as_object).is_some() {
            return Wire::OpenAi;
        }
        if tool.get("input_schema").is_some() {
            return Wire::Anthropic;
        }
    }
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some()
            || message.get("role").and_then(Value::as_str) == Some("tool")
        {
            return Wire::OpenAi;
        }
        let blocked = message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|b| b.get("type").and_then(Value::as_str))
            .any(|t| matches!(t, "tool_use" | "tool_result"));
        if blocked {
            return Wire::Anthropic;
        }
    }
    if path.contains("/chat/completions") {
        Wire::OpenAi
    } else {
        Wire::Anthropic
    }
}

/// Where the root is in its two-step script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootStep {
    /// Nothing has been delegated yet: call `spawn`.
    Delegate,
    /// The conversation already carries the result of that call: say something and stop.
    Finish,
}

/// Where the child is in its three-step script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStep {
    /// No edit yet: emit the `exec`/`apply_patch` call.
    ApplyPatch,
    /// The patch is in the transcript: return through marion's `report`.
    Report,
    /// Both have happened: emit the final message.
    Finish,
}

/// Where a `gemini` child is in its two-step script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiStep {
    /// Nothing has come back yet: call marion's `report`.
    Report,
    /// The transcript already carries that call's `functionResponse`: say something and stop.
    Finish,
}

/// Where an `opencode` child is in its two-step script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiStep {
    /// Nothing has come back yet: call marion's `report`.
    Report,
    /// The transcript already carries that call: say something and stop.
    Finish,
}

/// What a Gemini request is asking for, decided by shape alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiKind {
    /// A real turn: carries a non-empty `tools` array.
    ScriptedTurn,
    /// The CLI's model-routing classifier call. Answered with a fixed stub.
    RouterProbe,
}

/// Classify a Gemini `generateContent` request.
///
/// Same rule and same reasoning as [`classify_anthropic`]: a request carrying no tools is not a
/// turn we have a script for, and treating it as one is the failure this function exists to
/// prevent. On this wire the no-tools request has a name and a measured cost. S12: with no explicit
/// `-m` the CLI first asks `gemini-3.1-flash-lite`, over **non-streaming `:generateContent`**, for
/// a structured routing verdict — and a canned reply it could not use made it retry five times and
/// then hang, with no error printed anywhere.
///
/// marion's adapter always passes an explicit `-m`, so in a marion run this arm should never fire.
/// It exists so that when it does fire — an operator running the CLI by hand, a future model alias
/// that re-enables routing — the answer is a well-formed terminal response and the client gives up
/// on routing rather than hanging. **The verdict payload is a guess**: S12 recorded that the
/// classifier expects "a structured routing verdict" but never captured its schema, which is why
/// [`Script::gemini_router_verdict`] is a field an adapter can retarget rather than a constant.
///
/// Note this shares its shape with the trap S12 records for `trust: true`: a gemini child whose MCP
/// server is untrusted has its tools stripped from the request body entirely, so it too would land
/// here. That is the correct outcome — there is no tool to script a call to.
pub fn classify_gemini(body: &Value) -> GeminiKind {
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    if has_tools {
        GeminiKind::ScriptedTurn
    } else {
        GeminiKind::RouterProbe
    }
}

/// Decide the gemini child's step from the `contents` it sent us.
///
/// The signal is a `functionCall` or `functionResponse` **part** naming `report_tool`. Structured
/// evidence only, for the reason [`classify_child`] gives: the tool's name is also in the request's
/// `tools` declaration and in the system instruction on every single turn, so any substring scan
/// would answer `Finish` to turn one and the child would never call anything.
///
/// The name is a parameter rather than a constant because the model-facing spelling is the
/// harness's, not marion's ([`Script::gemini_report_tool`]).
pub fn classify_gemini_step(body: &Value, report_tool: &str) -> GeminiStep {
    let called = gemini_parts(body).any(|part| {
        ["functionCall", "functionResponse"].iter().any(|key| {
            part.get(key)
                .and_then(|c| c.get("name"))
                .and_then(Value::as_str)
                == Some(report_tool)
        })
    });
    if called {
        GeminiStep::Finish
    } else {
        GeminiStep::Report
    }
}

fn gemini_parts(body: &Value) -> impl Iterator<Item = &Value> {
    body.get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|content| content.get("parts").and_then(Value::as_array))
        .flatten()
}

/// Decide the opencode child's step from the `messages` it sent us.
///
/// The signal is an assistant message whose `tool_calls[].function.name` is `report_tool`, or the
/// `role: "tool"` message answering it. Chat Completions is stateless, so opencode replays the
/// whole transcript on every request and the evidence is stable once present — the same property
/// [`classify_root`] relies on, and the reason neither classifier needs to remember anything.
///
/// Structured evidence only, again: `report_tool` is named in `tools[]` on every turn.
pub fn classify_openai_step(body: &Value, report_tool: &str) -> OpenAiStep {
    let called = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|message| {
            let calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|call| {
                    call.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        == Some(report_tool)
                });
            let answered = message.get("role").and_then(Value::as_str) == Some("tool")
                && message.get("name").and_then(Value::as_str) == Some(report_tool);
            calls || answered
        });
    if called {
        OpenAiStep::Finish
    } else {
        OpenAiStep::Report
    }
}

/// Decide the root's step from the conversation it sent us.
///
/// The signal is a `tool_result` block anywhere in `messages`. Claude Code replays the whole
/// conversation on every turn, so once `spawn` has returned, its result is present on this and
/// every later request — which makes the predicate stable rather than edge-triggered.
pub fn classify_root(body: &Value) -> RootStep {
    let has_result = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("content").and_then(Value::as_array))
        .flatten()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
    if has_result {
        RootStep::Finish
    } else {
        RootStep::Delegate
    }
}

/// Decide the child's step from the `input` array it sent us.
///
/// Only *call* and *call output* items count as evidence. A substring scan would be wrong: under
/// code mode codex ships its tool catalogue inside `input` as an `additional_tools` developer
/// message (S6), so the words `exec` and `report` appear on turn one already.
pub fn classify_child(body: &Value) -> ChildStep {
    let input: Vec<&Value> = body
        .get("input")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    if input
        .iter()
        .any(|it| is_echo_of(it, REPORT_CALL_ID) || is_report_call(it))
    {
        ChildStep::Finish
    } else if input
        .iter()
        .any(|it| is_echo_of(it, PATCH_CALL_ID) || is_custom_tool_item(it))
    {
        ChildStep::Report
    } else {
        ChildStep::ApplyPatch
    }
}

/// Our own `call_id`, quoted back to us on a later turn — the most precise possible evidence,
/// since this provider authored the id.
fn is_echo_of(item: &Value, call_id: &str) -> bool {
    item.get("call_id").and_then(Value::as_str) == Some(call_id)
}

fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// A call to marion's `report` under either spelling S6 found: the `{name, namespace}` form codex
/// dispatches on, or the `mcp_tool_call` item it surfaces the result as.
fn is_report_call(item: &Value) -> bool {
    match item_type(item) {
        "function_call" => item.get("name").and_then(Value::as_str) == Some("report"),
        "mcp_tool_call" => item.get("tool").and_then(Value::as_str) == Some("report"),
        _ => false,
    }
}

fn is_custom_tool_item(item: &Value) -> bool {
    matches!(
        item_type(item),
        "custom_tool_call" | "custom_tool_call_output"
    )
}

/// The canned behaviour of one M1 run, as data.
///
/// Held as a struct rather than hard-coded constants so an integration test can point the child's
/// patch at a path that actually exists in its own fixture repo.
#[derive(Debug, Clone)]
pub struct Script {
    /// The tool the root calls on its first turn, in the harness's own spelling.
    ///
    /// Parameterised so a test can aim the root at a verb that is **not** in
    /// `ROOT_ALLOWED_TOOLS` — which is the only way to provoke a real inbound `can_use_tool`
    /// frame from the CLI (S9, design §11 item 14). The M1 hop leaves it at `spawn`.
    pub root_tool: String,
    /// Arguments the root passes to [`Script::root_tool`].
    pub root_tool_input: Value,
    /// The root's closing text turn.
    pub root_final_text: String,
    /// The patch the child feeds to `tools.apply_patch` (a string — S6).
    pub child_patch: String,
    /// Code-mode JavaScript to send *instead of* the `apply_patch` wrapper on the child's first
    /// `exec` call.
    ///
    /// Parameterised for the same reason as [`Script::root_tool`]: §9's M1 criterion 6 needs the
    /// child's tool call to be a `tools.exec_command` that is **still running when `exec` yields**
    /// (§11 item 18 case B), which is the only shape that leaks a descendant at timeout expiry.
    /// `None` keeps the M1 hop's patch step.
    pub child_exec_js: Option<String>,
    /// The `narrative` the child passes to marion's `report`.
    pub child_narrative: String,
    /// The child's final assistant message. Under `--output-schema` this is delivered verbatim to
    /// `--output-last-message`, so the default is a schema document, not prose.
    pub child_final_text: String,
    /// marion's `report`, in the spelling a **gemini** child sees: `mcp_<server>_<tool>` with
    /// single underscores (S12). A field rather than a constant for the same reason
    /// [`Script::root_tool`] is one — and because the server alias is the operator's to choose.
    pub gemini_report_tool: String,
    /// Arguments the gemini child passes to [`Script::gemini_report_tool`].
    pub gemini_report_args: Value,
    /// The gemini child's closing text turn.
    pub gemini_final_text: String,
    /// What to answer a Gemini request that declares no tools — in practice the CLI's model-routing
    /// classifier call. See [`classify_gemini`]: the schema of a real verdict was never captured,
    /// so this is a placeholder whose only guaranteed property is that it is well formed and
    /// terminal. Parameterised so an adapter that *does* learn the schema can supply it without
    /// touching this crate.
    pub gemini_router_verdict: String,
    /// marion's `report`, in the spelling an **opencode** child sees: `<serverName>_<toolName>`
    /// (S13). Distinct from the gemini spelling, and from the unprefixed `report` that opencode
    /// sends over JSON-RPC to the MCP server itself.
    pub openai_report_tool: String,
    /// Arguments the opencode child passes to [`Script::openai_report_tool`].
    pub openai_report_args: Value,
    /// The opencode child's closing text turn.
    pub openai_final_text: String,
}

impl Default for Script {
    fn default() -> Self {
        let narrative = "Added the M1 marker file under src/.";
        Self {
            root_tool: "mcp__marion__spawn".to_string(),
            root_tool_input: json!({
                "agent_type": "codex-impl",
                "prompt": "Add the M1 marker file under src/ and report back.",
                "acceptance_criteria": ["a file exists under src/ containing the M1 marker"],
                "writable_scope": ["src/**"],
            }),
            root_final_text: "The child completed the task and reported back. M1 hop done."
                .to_string(),
            child_patch: "*** Begin Patch\n*** Add File: src/marion_m1.txt\n\
                          +marion M1: written by the canned codex child\n*** End Patch"
                .to_string(),
            child_exec_js: None,
            child_narrative: narrative.to_string(),
            child_final_text: json!({"narrative": narrative, "result_commits": []}).to_string(),
            gemini_report_tool: "mcp_marion_report".to_string(),
            gemini_report_args: json!({"narrative": narrative}),
            gemini_final_text: "Reported back through marion. Done.".to_string(),
            gemini_router_verdict: json!({"model_choice": "flash", "reasoning": "canned"})
                .to_string(),
            openai_report_tool: "marion_report".to_string(),
            openai_report_args: json!({"narrative": narrative}),
            openai_final_text: "Reported back through marion. Done.".to_string(),
        }
    }
}

impl Script {
    /// Produce the SSE body for one request, on whichever wire it arrived.
    pub fn respond(&self, wire: Wire, body: &Value) -> String {
        match wire {
            Wire::Anthropic => self.respond_anthropic(body),
            Wire::Responses => self.respond_responses(body),
            Wire::Gemini { streaming } => self.respond_gemini(body, streaming),
            Wire::OpenAi => self.respond_openai(body),
        }
    }

    fn respond_anthropic(&self, body: &Value) -> String {
        match classify_anthropic(body) {
            RequestKind::SessionTitle => anthropic::session_title_stub(),
            RequestKind::ScriptedTurn => match classify_root(body) {
                RootStep::Delegate => anthropic::tool_use_turn(
                    &self.root_tool,
                    ROOT_TOOL_USE_ID,
                    &self.root_tool_input,
                ),
                RootStep::Finish => anthropic::text_turn(&self.root_final_text),
            },
        }
    }

    fn respond_responses(&self, body: &Value) -> String {
        match classify_child(body) {
            ChildStep::ApplyPatch => match &self.child_exec_js {
                Some(js) => responses::exec_call(js, PATCH_CALL_ID),
                None => responses::apply_patch_call(&self.child_patch, PATCH_CALL_ID),
            },
            ChildStep::Report => responses::report_call(&self.child_narrative, REPORT_CALL_ID),
            ChildStep::Finish => responses::final_message(&self.child_final_text),
        }
    }

    fn respond_gemini(&self, body: &Value, streaming: bool) -> String {
        match classify_gemini(body) {
            GeminiKind::RouterProbe => gemini::text_turn(&self.gemini_router_verdict, streaming),
            GeminiKind::ScriptedTurn => {
                match classify_gemini_step(body, &self.gemini_report_tool) {
                    GeminiStep::Report => gemini::function_call_turn(
                        &self.gemini_report_tool,
                        &self.gemini_report_args,
                        streaming,
                    ),
                    GeminiStep::Finish => gemini::text_turn(&self.gemini_final_text, streaming),
                }
            }
        }
    }

    fn respond_openai(&self, body: &Value) -> String {
        match classify_openai_step(body, &self.openai_report_tool) {
            OpenAiStep::Report => openai::tool_call_turn(
                &self.openai_report_tool,
                REPORT_CALL_ID,
                &self.openai_report_args,
            ),
            OpenAiStep::Finish => openai::text_turn(&self.openai_final_text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_turn() -> Value {
        json!({
            "model": "claude",
            "tools": [{"name": "mcp__marion__spawn"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "delegate"}]}]
        })
    }

    fn after_spawn_returned() -> Value {
        let mut b = user_turn();
        b["messages"].as_array_mut().unwrap().push(json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": ROOT_TOOL_USE_ID,
                         "name": "mcp__marion__spawn", "input": {}}]
        }));
        b["messages"].as_array_mut().unwrap().push(json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": ROOT_TOOL_USE_ID,
                         "content": "{\"state\":\"ok\"}"}]
        }));
        b
    }

    #[test]
    fn the_root_delegates_before_anything_has_come_back() {
        assert_eq!(classify_root(&user_turn()), RootStep::Delegate);
    }

    #[test]
    fn the_root_stops_once_the_transcript_carries_the_spawn_result() {
        assert_eq!(classify_root(&after_spawn_returned()), RootStep::Finish);
    }

    #[test]
    fn root_steps_are_a_function_of_the_body_not_of_arrival_order() {
        // Replay the *second* turn first. A counting provider would answer it with `spawn`,
        // spawn a second child, and never end the run.
        let s = Script::default();
        let second = s.respond(Wire::Anthropic, &after_spawn_returned());
        let first = s.respond(Wire::Anthropic, &user_turn());
        assert!(
            second.contains("end_turn"),
            "the later turn must still end the run"
        );
        assert!(
            first.contains("mcp__marion__spawn"),
            "the earlier turn must still delegate"
        );
    }

    #[test]
    fn the_concurrent_title_request_never_consumes_the_spawn_turn() {
        // The exact failure the crate docs describe: title request racing turn one.
        let title = json!({
            "model": "claude", "tools": [],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "output_config": {"format": {"type": "json_schema"}}
        });
        let out = Script::default().respond(Wire::Anthropic, &title);
        assert!(!out.contains("mcp__marion__spawn"));
        assert!(
            out.contains("marion m1"),
            "it gets the fixed title stub instead"
        );
    }

    fn child_input(items: Value) -> Value {
        json!({"model": "gpt-5.6-sol", "input": items})
    }

    #[test]
    fn the_child_patches_first() {
        let developer_catalogue = json!([{
            "type": "message", "role": "developer",
            "content": [{"type": "input_text",
                         "text": "additional_tools: exec, mcp__marion__report"}]
        }]);
        assert_eq!(
            classify_child(&child_input(developer_catalogue)),
            ChildStep::ApplyPatch,
            "the tool catalogue names exec and report on turn one; naming is not evidence of use"
        );
    }

    #[test]
    fn the_child_reports_once_the_patch_call_is_in_the_transcript() {
        let items = json!([
            {"type": "custom_tool_call", "call_id": PATCH_CALL_ID, "name": "exec", "input": "…"},
            {"type": "custom_tool_call_output", "call_id": PATCH_CALL_ID, "output": "ok"},
        ]);
        assert_eq!(classify_child(&child_input(items)), ChildStep::Report);
    }

    #[test]
    fn the_child_finishes_once_report_has_been_called() {
        let items = json!([
            {"type": "custom_tool_call", "call_id": PATCH_CALL_ID, "name": "exec", "input": "…"},
            {"type": "function_call", "call_id": REPORT_CALL_ID, "name": "report",
             "namespace": "mcp__marion", "arguments": "{}"},
            {"type": "function_call_output", "call_id": REPORT_CALL_ID, "output": "ok"},
        ]);
        assert_eq!(classify_child(&child_input(items)), ChildStep::Finish);
    }

    #[test]
    fn a_report_surfaced_as_an_mcp_tool_call_item_also_counts() {
        // S6 shows codex echoing the MCP result as an `mcp_tool_call` item rather than a
        // function_call/output pair; missing that spelling would loop the child forever.
        let items = json!([
            {"type": "custom_tool_call", "call_id": PATCH_CALL_ID, "name": "exec"},
            {"type": "mcp_tool_call", "server": "marion", "tool": "report", "result": "ok"},
        ]);
        assert_eq!(classify_child(&child_input(items)), ChildStep::Finish);
    }

    #[test]
    fn the_child_script_emits_a_string_patch_then_report_then_a_schema_document() {
        let s = Script::default();
        assert!(
            s.respond(Wire::Responses, &child_input(json!([])))
                .contains("tools.apply_patch(\\\"")
        );
        let items = json!([{"type": "custom_tool_call", "call_id": PATCH_CALL_ID}]);
        assert!(
            s.respond(Wire::Responses, &child_input(items))
                .contains("\"namespace\":\"mcp__marion\"")
        );
        let done = json!([{"type": "mcp_tool_call", "tool": "report"}]);
        assert!(
            s.respond(Wire::Responses, &child_input(done))
                .contains("result_commits")
        );
    }

    #[test]
    fn a_scripted_exec_body_replaces_the_patch_step_and_keeps_its_call_id() {
        // The call_id has to survive the substitution: `classify_child` recognises the step's echo
        // by that id, so a different one would loop the child on `ApplyPatch` forever.
        let s = Script {
            child_exec_js: Some("await tools.exec_command({cmd: \"sleep 900\"});".into()),
            ..Script::default()
        };
        let out = s.respond(Wire::Responses, &child_input(json!([])));
        assert!(out.contains("exec_command"));
        assert!(!out.contains("apply_patch"));
        assert!(out.contains(PATCH_CALL_ID));
    }

    #[test]
    fn wires_are_told_apart_by_body_first_then_path() {
        assert_eq!(
            classify_wire("/x", &json!({"messages": []})),
            Some(Wire::Anthropic)
        );
        assert_eq!(
            classify_wire("/x", &json!({"input": []})),
            Some(Wire::Responses)
        );
        assert_eq!(
            classify_wire("/v1/messages", &json!({})),
            Some(Wire::Anthropic)
        );
        assert_eq!(
            classify_wire("/v1/responses", &json!({})),
            Some(Wire::Responses)
        );
        assert_eq!(classify_wire("/health", &json!({})), None);
    }

    // --- the four wires ----------------------------------------------------------------------

    /// The four measured request shapes, each with the path its harness actually uses.
    fn measured_bodies() -> [(&'static str, &'static str, Value); 4] {
        [
            (
                "anthropic",
                "/v1/messages",
                json!({
                    "model": "claude", "max_tokens": 1024, "stream": true,
                    "tools": [{"name": "mcp__marion__spawn", "input_schema": {"type": "object"}}],
                    "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}],
                }),
            ),
            (
                "responses",
                "/v1/responses",
                json!({"model": "gpt-5.6-sol", "input": [], "stream": true}),
            ),
            (
                "gemini",
                "/v1beta/models/gemini-3.5-flash:streamGenerateContent?alt=sse",
                json!({
                    "contents": [{"role": "user", "parts": [{"text": "go"}]}],
                    "systemInstruction": {"parts": [{"text": "you are"}]},
                    "tools": [{"functionDeclarations": [{"name": "mcp_marion_report"}]}],
                }),
            ),
            (
                "openai",
                "/v1/chat/completions",
                json!({
                    "model": "fake-1", "stream": true,
                    "tools": [{"type": "function", "function": {
                        "name": "marion_report", "parameters": {"type": "object"}}}],
                    "messages": [{"role": "user", "content": "go"}],
                }),
            ),
        ]
    }

    #[test]
    fn all_four_wires_classify_unambiguously_and_no_body_answers_to_two() {
        let bodies = measured_bodies();
        for (expected, path, body) in &bodies {
            let wire = classify_wire(path, body).expect("every measured body routes");
            assert_eq!(&wire_name(wire), expected, "on its own path: {path}");
            // The body is the evidence: every *other* harness's path must not change the verdict.
            for (_, other_path, _) in &bodies {
                let under_other = classify_wire(other_path, body).expect("still routes");
                assert_eq!(
                    wire_name(under_other),
                    *expected,
                    "{expected} body re-routed by the path {other_path}"
                );
            }
        }
        // And the four verdicts are four, not three with a collision.
        let mut names: Vec<&str> = bodies
            .iter()
            .map(|(_, p, b)| wire_name(classify_wire(p, b).unwrap()))
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 4, "two shapes collapsed onto one wire");
    }

    #[test]
    fn the_two_wires_that_share_messages_are_split_on_their_tool_declarations() {
        // The only genuine ambiguity in the set: both bodies carry a top-level `messages` array,
        // and each arrives on the *other* one's path. Shape must win both times.
        let anthropic = json!({
            "max_tokens": 1024,
            "tools": [{"name": "t", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}],
        });
        let openai = json!({
            "tools": [{"type": "function", "function": {"name": "t", "parameters": {}}}],
            "messages": [{"role": "user", "content": "go"}],
        });
        assert_eq!(
            classify_wire("/v1/chat/completions", &anthropic),
            Some(Wire::Anthropic),
            "`input_schema` is Anthropic's tool shape whatever the path says"
        );
        assert_eq!(
            classify_wire("/v1/messages", &openai),
            Some(Wire::OpenAi),
            "`tools[].function` is OpenAI's tool shape whatever the path says"
        );
    }

    #[test]
    fn the_transcript_splits_them_when_the_tool_declarations_do_not() {
        // A turn whose tools have been stripped still carries its own transcript shape.
        let anthropic = json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "x"}]}]});
        let openai = json!({"messages": [
            {"role": "assistant", "tool_calls": [{"id": "c", "type": "function",
                                                  "function": {"name": "x", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c", "content": "ok"}]});
        assert_eq!(classify_wire("/x", &anthropic), Some(Wire::Anthropic));
        assert_eq!(classify_wire("/x", &openai), Some(Wire::OpenAi));
    }

    #[test]
    fn a_bare_messages_array_still_falls_back_to_anthropic_unless_the_path_says_otherwise() {
        // Neither harness sends this; the tie is broken by configuration, consulted last.
        assert_eq!(
            classify_wire("/x", &json!({"messages": []})),
            Some(Wire::Anthropic)
        );
        assert_eq!(
            classify_wire("/v1/chat/completions", &json!({"messages": []})),
            Some(Wire::OpenAi)
        );
    }

    #[test]
    fn the_gemini_endpoint_selects_the_framing_and_only_the_framing() {
        let body = json!({"contents": []});
        assert_eq!(
            classify_wire("/v1beta/models/m:streamGenerateContent?alt=sse", &body),
            Some(Wire::Gemini { streaming: true })
        );
        assert_eq!(
            classify_wire("/v1beta/models/m:generateContent", &body),
            Some(Wire::Gemini { streaming: false })
        );
        // The model id in the path is never matched: S12 measured `-m gemini-2.5-flash` arriving
        // as `gemini-3.5-flash`, so any id would do here.
        assert_eq!(
            classify_wire("/v1beta/models/anything-at-all:generateContent", &body),
            Some(Wire::Gemini { streaming: false })
        );
    }

    // --- the gemini child --------------------------------------------------------------------

    fn gemini_turn(parts: Value) -> Value {
        json!({
            "contents": [{"role": "user", "parts": [{"text": "do the thing"}]},
                         {"role": "model", "parts": parts}],
            "systemInstruction": {"parts": [{"text": "tools: mcp_marion_report"}]},
            "tools": [{"functionDeclarations": [{"name": "mcp_marion_report"}]}],
        })
    }

    fn gemini_first_turn() -> Value {
        json!({
            "contents": [{"role": "user", "parts": [{"text": "do the thing"}]}],
            "systemInstruction": {"parts": [{"text": "you may call mcp_marion_report"}]},
            "tools": [{"functionDeclarations": [{"name": "mcp_marion_report"}]}],
        })
    }

    fn gemini_after_report() -> Value {
        let mut b = gemini_turn(json!([{"functionCall": {"name": "mcp_marion_report",
                                                         "args": {"narrative": "x"}}}]));
        b["contents"].as_array_mut().unwrap().push(json!({
            "role": "user",
            "parts": [{"functionResponse": {"name": "mcp_marion_report",
                                            "response": {"state": "ok"}}}]
        }));
        b
    }

    #[test]
    fn the_gemini_child_reports_before_anything_has_come_back() {
        assert_eq!(
            classify_gemini_step(&gemini_first_turn(), "mcp_marion_report"),
            GeminiStep::Report,
            "the tool is named in `tools` and in the system instruction on turn one already"
        );
    }

    #[test]
    fn the_gemini_child_stops_once_the_transcript_carries_the_function_response() {
        assert_eq!(
            classify_gemini_step(&gemini_after_report(), "mcp_marion_report"),
            GeminiStep::Finish
        );
    }

    #[test]
    fn a_gemini_call_to_some_other_tool_is_not_evidence_of_reporting() {
        // A trusted gemini child also has its core tools; one of them returning proves nothing.
        let other = gemini_turn(json!([{"functionCall": {"name": "read_file", "args": {}}}]));
        assert_eq!(
            classify_gemini_step(&other, "mcp_marion_report"),
            GeminiStep::Report
        );
    }

    #[test]
    fn gemini_steps_are_a_function_of_the_body_not_of_arrival_order() {
        // Replay the *second* turn first. A counting provider would answer it with another
        // `report` call, and the child would report twice and never finish.
        let s = Script::default();
        let second = s.respond(Wire::Gemini { streaming: true }, &gemini_after_report());
        let first = s.respond(Wire::Gemini { streaming: true }, &gemini_first_turn());
        assert!(
            second.contains(&s.gemini_final_text) && !second.contains("functionCall"),
            "the later turn must still end the run"
        );
        assert!(
            first.contains("\"functionCall\""),
            "the earlier turn must still report"
        );
        assert!(first.contains("mcp_marion_report"));
    }

    #[test]
    fn the_gemini_router_probe_is_answered_rather_than_left_to_hang() {
        // S12 gotcha (a): with no explicit `-m` the CLI asks a flash-lite model, over non-streaming
        // `:generateContent`, for a routing verdict. A reply it cannot use cost 5 retries and a
        // hang. It has no tools, which is exactly what tells it apart from a scripted turn.
        let probe = json!({
            "contents": [{"role": "user", "parts": [{"text": "route this"}]}],
            "generationConfig": {"responseMimeType": "application/json"},
        });
        assert_eq!(classify_gemini(&probe), GeminiKind::RouterProbe);
        assert_eq!(
            classify_gemini(&gemini_first_turn()),
            GeminiKind::ScriptedTurn
        );

        let out = Script::default().respond(Wire::Gemini { streaming: false }, &probe);
        assert!(
            !out.contains("functionCall"),
            "the classifier must never be handed the scripted tool call"
        );
        let v: Value = serde_json::from_str(&out).expect(":generateContent answers plain JSON");
        assert_eq!(
            v["candidates"][0]["finishReason"], "STOP",
            "a terminal answer is the whole point: an unusable one is a five-retry hang"
        );
    }

    // --- the opencode child ------------------------------------------------------------------

    fn openai_first_turn() -> Value {
        json!({
            "model": "fake-1", "stream": true,
            "tools": [{"type": "function", "function": {
                "name": "marion_report", "parameters": {"type": "object"}}}],
            "messages": [{"role": "system", "content": "you may call marion_report"},
                         {"role": "user", "content": "do the thing"}],
        })
    }

    fn openai_after_report() -> Value {
        let mut b = openai_first_turn();
        let messages = b["messages"].as_array_mut().unwrap();
        messages.push(json!({
            "role": "assistant", "content": null,
            "tool_calls": [{"id": REPORT_CALL_ID, "type": "function", "function": {
                "name": "marion_report", "arguments": "{\"narrative\":\"x\"}"}}],
        }));
        messages.push(json!({
            "role": "tool", "tool_call_id": REPORT_CALL_ID, "name": "marion_report",
            "content": "{\"state\":\"ok\"}",
        }));
        b
    }

    #[test]
    fn the_opencode_child_reports_before_anything_has_come_back() {
        assert_eq!(
            classify_openai_step(&openai_first_turn(), "marion_report"),
            OpenAiStep::Report,
            "the tool is named in `tools` and in the system message on turn one already"
        );
    }

    #[test]
    fn the_opencode_child_stops_once_the_transcript_carries_the_tool_call() {
        assert_eq!(
            classify_openai_step(&openai_after_report(), "marion_report"),
            OpenAiStep::Finish
        );
    }

    #[test]
    fn an_opencode_call_to_some_other_tool_is_not_evidence_of_reporting() {
        let mut b = openai_first_turn();
        b["messages"].as_array_mut().unwrap().push(json!({
            "role": "assistant",
            "tool_calls": [{"id": "c9", "type": "function",
                            "function": {"name": "read", "arguments": "{}"}}],
        }));
        assert_eq!(
            classify_openai_step(&b, "marion_report"),
            OpenAiStep::Report
        );
    }

    #[test]
    fn openai_steps_are_a_function_of_the_body_not_of_arrival_order() {
        // Replay the *second* turn first, as with every other wire in this file.
        let s = Script::default();
        let second = s.respond(Wire::OpenAi, &openai_after_report());
        let first = s.respond(Wire::OpenAi, &openai_first_turn());
        assert!(
            second.contains("\"finish_reason\":\"stop\"") && !second.contains("tool_calls\":["),
            "the later turn must still end the run"
        );
        assert!(
            first.contains("\"finish_reason\":\"tool_calls\"") && first.contains("marion_report"),
            "the earlier turn must still report"
        );
    }

    #[test]
    fn each_wire_gets_its_own_spelling_of_the_same_report_tool() {
        // §5.4's per-harness-spelling rule, asserted as one statement: four wires, four names, no
        // translation anywhere between them.
        let s = Script::default();
        assert_eq!(s.gemini_report_tool, "mcp_marion_report");
        assert_eq!(s.openai_report_tool, "marion_report");
        let gemini_out = s.respond(Wire::Gemini { streaming: true }, &gemini_first_turn());
        let openai_out = s.respond(Wire::OpenAi, &openai_first_turn());
        assert!(!gemini_out.contains("mcp__marion__report"));
        assert!(!gemini_out.contains("marion_report\","));
        assert!(!openai_out.contains("mcp_marion_report"));
        assert!(
            !openai_out.contains("\"name\":\"report\""),
            "the unprefixed name is the MCP layer's, not this wire's"
        );
    }

    #[test]
    fn retargeting_the_tool_name_retargets_both_the_call_and_the_step_predicate() {
        // Data, not constants: an operator who renames the MCP server alias must not have to
        // patch this crate, and the classifier must follow the emitter.
        let s = Script {
            gemini_report_tool: "mcp_other_report".into(),
            openai_report_tool: "other_report".into(),
            ..Script::default()
        };
        assert!(
            s.respond(Wire::Gemini { streaming: true }, &gemini_first_turn())
                .contains("mcp_other_report")
        );
        assert_eq!(
            classify_gemini_step(&gemini_after_report(), &s.gemini_report_tool),
            GeminiStep::Report,
            "a transcript naming the *old* tool is no longer evidence"
        );
        assert!(
            s.respond(Wire::OpenAi, &openai_first_turn())
                .contains("other_report")
        );
        assert_eq!(
            classify_openai_step(&openai_after_report(), &s.openai_report_tool),
            OpenAiStep::Report
        );
    }
}
