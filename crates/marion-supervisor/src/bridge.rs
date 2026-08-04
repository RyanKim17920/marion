//! The stdio MCP bridge (design §5.4, §6.1 step 8).
//!
//! marion does **not** spawn this process — the harness does, from a server declaration marion
//! wrote at config-injection time. That is why the per-node token rides the declaration rather
//! than an inherited fd or the bridge's stdin: marion is not the parent, and stdin is already the
//! JSON-RPC transport.
//!
//! Frame shapes follow `tests/fixtures/s6/mcp-server-frames.jsonl`, captured from a real
//! `codex exec` 0.146.0.
//!
//! **The bridge must be idempotent across repeated startup**: S6 observed codex issuing *two* full
//! `initialize` + `tools/list` sequences for a single `exec` run.

use marion_core::contract::{ExitStatus, TaskContract};
use serde_json::{Value, json};

use crate::spawn::SpawnError;

/// Protocol version we answer `initialize` with. Codex 0.146.0 offers `2025-06-18`; MCP requires
/// the server to reply with a version it supports, not to echo the client's.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A decoded inbound request.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Initialize {
        id: Value,
    },
    ToolsList {
        id: Value,
    },
    ToolsCall {
        id: Value,
        name: String,
        arguments: Value,
    },
    /// Notifications carry no id and expect no reply.
    Notification,
    /// Anything else with an id: answered with method-not-found rather than ignored, so a client
    /// waiting on it is never left hanging.
    Unknown {
        id: Value,
        method: String,
    },
}

pub fn parse(line: &str) -> Option<Request> {
    let v: Value = serde_json::from_str(line).ok()?;
    let method = v.get("method").and_then(Value::as_str)?;
    let id = v.get("id").cloned();
    Some(match (method, id) {
        ("initialize", Some(id)) => Request::Initialize { id },
        ("tools/list", Some(id)) => Request::ToolsList { id },
        ("tools/call", Some(id)) => Request::ToolsCall {
            id,
            name: v
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: v
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
        },
        (m, _) if m.starts_with("notifications/") => Request::Notification,
        (m, Some(id)) => Request::Unknown {
            id,
            method: m.to_string(),
        },
        (_, None) => Request::Notification,
    })
}

/// marion's tool surface. `spawn` is child-creating; `report` is the child's return path.
///
/// Names here are the **MCP tool names**. The surface spelling differs per harness — Claude Code
/// prefixes to `mcp__marion__spawn`, and Codex exposes the same tool to the model as the
/// JavaScript identifier `tools.mcp__marion__report` under code mode (S6) — but neither spelling
/// belongs in this declaration.
///
/// # Why this is a constant and not a function of the node
///
/// The node's depth *is* available here — it rides the same `env` block as `MARION_AGENT_ID`
/// (`marion_harness::DEPTH_ENV`) — so a bridge serving a node at its type's `max_depth` could
/// simply not offer `spawn`, and one serving a root could not offer `report`. That was considered
/// and **rejected**, on three grounds:
///
/// 1. **§3.1 wants a refusal that names the bound.** A `spawn` past `max_depth` "is refused with a
///    spawn error, never silently clamped". A verb that is merely *absent* carries no bound, no
///    value and no sentence: the node cannot tell "marion has no `spawn`" from "I may not spawn",
///    and nothing anywhere reports why. That is the §12 silent-failure shape this codebase keeps
///    legislating against, traded for the loud refusal `run::run_spawn` now returns.
/// 2. **`tools/list` is answered once, at startup; the gate is evaluated per call.** Availability
///    would be a snapshot and permission a live decision, so the two could only ever agree or lag
///    — a second source of truth for a rule that must exist in the first place anyway. §9 warns
///    about exactly that kind of drift.
/// 3. **§3.1 and §9 already sanction the two axes differing.** §9 notes the root's allowlist being
///    wider than the declared surface is "not a bug". A uniform declaration plus a per-call gate is
///    a shape the design states, not a gap in it.
///
/// **`report` on a root is the one inconsistency left standing, knowingly.** §7.6 says a root has
/// no contract and cannot `report`, yet `main::handle_tool_call` answers `report recorded` to a
/// root today. Closing it means the bridge answering a root's `report` with an error — and that
/// answer is *inside* `tests/fixtures/s9/can-use-tool-allow.stdout.jsonl`, a committed recording
/// of a real 2.1.220 run that `permission_round_trip` replays and asserts still matches. Correcting
/// the behaviour therefore requires re-recording that fixture, which this change does not touch.
/// It is a real defect, and it is written down here rather than papered over.
pub fn tools() -> Value {
    json!([
        {
            "name": "spawn",
            "description": "Delegate a task to a child agent. Blocks until the child reaches a \
                            terminal state, then returns the completed task contract.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_type": {"type": "string"},
                    // Optional: omitted, the agent type's own default is used (§3.1). Named in
                    // marion's vocabulary — the adapter maps it to the harness's spelling.
                    "model": {"type": "string"},
                    "prompt": {"type": "string"},
                    "acceptance_criteria": {"type": "array", "items": {"type": "string"}},
                    "verification": {"type": "array", "items": {"type": "string"}},
                    "writable_scope": {"type": "array", "items": {"type": "string"}},
                    "name": {"type": "string"},
                    "isolation": {"type": "string", "enum": ["worktree", "shared-cwd", "remote"]},
                    "timeout_secs": {"type": "integer"},
                    "allow_concurrent_writes": {"type": "boolean"},
                    "background": {"type": "boolean"}
                },
                "required": ["agent_type", "prompt", "acceptance_criteria"]
            }
        },
        {
            "name": "report",
            "description": "Return your result to marion. Call this exactly once when the task \
                            is done. Your final assistant message is NOT the return value.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "narrative": {"type": "string"},
                    "result_commits": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["narrative"]
            }
        }
    ])
}

pub fn initialize_result(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "marion", "version": env!("CARGO_PKG_VERSION")}}})
}

pub fn tools_list_result(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}})
}

pub fn tool_result(id: &Value, text: &str, is_error: bool) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": [{"type": "text", "text": text}], "isError": is_error}})
}

/// Longest first line marion will put in front of a `spawn` result, in bytes.
///
/// **This is what keeps §6.7's arithmetic intact.** The contract has already been through
/// [`marion_core::cap::cap_for_return`] by the time it reaches here, so nothing this module writes
/// can change which cap tier it landed in — the harness's own words are already *inside* the
/// contract, in `completion.exit.description`, and were counted there. What this module adds is a
/// second, shorter copy in the surrounding message, and an unbounded one could push the whole tool
/// result past the ~64 KB at which Claude Code 2.1.220 replaces it with a `<persisted-output>`
/// stub — the failure the cap exists to avoid, reintroduced one layer up. 1 KiB against §6.7's
/// 48 KiB backstop leaves the encoded contract two per cent of headroom it did not need, and a
/// diagnosis longer than 1 KiB is not a first line anyway. The full text always survives verbatim
/// in the contract printed underneath.
pub const SUMMARY_CAP: usize = 1024;

/// `s`, or its longest whole-character prefix under [`SUMMARY_CAP`] with an ellipsis.
///
/// The elision is marked rather than silent: a truncated diagnosis that looked complete would be
/// a worse lie than a long one.
fn bounded(s: &str) -> String {
    if s.len() <= SUMMARY_CAP {
        return s.to_string();
    }
    let mut i = SUMMARY_CAP;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &s[..i])
}

/// The one plain sentence a child's failure comes back as, or `None` when it succeeded.
///
/// The diagnosis is not rebuilt here. `spawn::build_contract` already layers marion's exit numbers,
/// the harness's own `StreamOutcome::failure` and a stderr preview into
/// `completion.exit.description`; this only puts a verb in front of it and hoists it out of the
/// JSON, so the parent reads it instead of hunting for it.
///
/// **`Unreported` gets its own sentence and does not say "failed".** §7.6 treats a child that never
/// called `report` as a distinct outcome — the point of the status is that marion will *"never
/// silently promote a status message to an answer"* — and a parent told "the child crashed" would
/// draw exactly the wrong conclusion about a child that may well have done the work and simply not
/// returned it.
///
/// A contract with no completion at all is not a failure: that is a live or unobserved run, which
/// M1's blocking `spawn` never returns, and inventing a failure line for it would change the
/// success path on a shape this function cannot diagnose.
fn failure_line(c: &TaskContract) -> Option<String> {
    let comp = c.completion.as_ref()?;
    let harness = c.child.harness;
    let head = match comp.status {
        ExitStatus::Ok => return None,
        ExitStatus::Unreported => {
            format!("marion: the {harness} child never called report, so it returned no answer")
        }
        ExitStatus::TimedOut => format!("marion: the {harness} child timed out"),
        ExitStatus::Cancelled => format!("marion: the {harness} child was cancelled"),
        ExitStatus::Killed => format!("marion: the {harness} child was killed"),
        ExitStatus::Failed => format!("marion: the {harness} child failed"),
    };
    Some(bounded(&format!("{head} — {}", comp.exit.description)))
}

/// The tool result for one `spawn`, in the three shapes a `spawn` can end in.
///
/// 1. **A child that finished `Ok`** returns exactly what it always did: the pretty-printed
///    contract, `isError: false`, byte for byte.
/// 2. **A child that ran and did not finish `Ok`** returns the same contract with a one-line
///    account of the failure above it, and `isError: true`.
/// 3. **A spawn that never launched** returns that same shape with no contract to print, and
///    `isError: true` — which it already did, and which is why the two now read alike.
///
/// # Why a failed child is `isError: true`
///
/// Because S9 measured what a harness does with one. `tests/fixtures/s9/README.md` records a real
/// 2.1.220 run in which marion answered a permission ask with `is_error: true`: the harness
/// rendered marion's message **verbatim**, listed the call under `permission_denials`, and **the
/// turn continued** — `terminal_reason: "completed"`, exit 0. An error-shaped tool result is how a
/// harness says "this call did not do what you asked", not how it aborts; the model reads the text
/// and decides. That is precisely the reading a failed child needs, and it is measured rather than
/// assumed.
///
/// The alternative — leaving a failed child `isError: false` — is the state this replaces, where a
/// `Failed` contract and an `Ok` contract were the same shape with the same flag and the parent had
/// to find `"status": "Failed"` inside a kilobyte of serialized JSON to tell them apart. §12 calls
/// that the silent-failure shape.
///
/// The contract still rides underneath in every case. It stops being the *message*; it does not
/// stop being the record, and what is persisted on disk is untouched either way.
///
/// # Why the launch path and the run path have the same sentence
///
/// They are the same conceptual outcome — "the child you asked for did not produce an answer" — and
/// they disagreed about it: one arrived as `isError: true` prose, the other as a `false` blob. Both
/// now open `marion: the <thing> child <verb> — <diagnosis>`, so a model that has learnt to read one
/// can read the other. `SpawnError`'s Display is used as-is, which is what keeps `Gate`'s bound and
/// value, `Duplex`'s `McpNeverReady`, `Scope` and the rest intact rather than flattened: the
/// wrapper supplies the verb, the variant supplies the diagnosis.
pub fn spawn_result(
    id: &Value,
    agent_type: &str,
    outcome: Result<TaskContract, SpawnError>,
) -> Value {
    match outcome {
        Ok(contract) => {
            let json = serde_json::to_string_pretty(&contract)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
            match failure_line(&contract) {
                None => tool_result(id, &json, false),
                Some(line) => tool_result(id, &format!("{line}\n\n{json}"), true),
            }
        }
        Err(e) => tool_result(
            id,
            &bounded(&format!(
                "marion: the {agent_type} child could not be launched — {e}"
            )),
            true,
        ),
    }
}

pub fn method_not_found(id: &Value, method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id,
           "error": {"code": -32601, "message": format!("no method {method}")}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_frames_a_real_codex_sends() {
        // Verbatim from tests/fixtures/s6/mcp-server-frames.jsonl.
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"codex-mcp-client","title":"Codex","version":"0.146.0"}}}"#;
        assert!(matches!(parse(init), Some(Request::Initialize { .. })));

        let listed = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"progressToken":0}}}"#;
        assert!(matches!(parse(listed), Some(Request::ToolsList { .. })));

        let called = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"report","arguments":{"narrative":"s6 probe"}}}"#;
        match parse(called) {
            Some(Request::ToolsCall {
                name, arguments, ..
            }) => {
                assert_eq!(name, "report");
                assert_eq!(arguments["narrative"], "s6 probe");
            }
            other => panic!("expected a tools/call, got {other:?}"),
        }

        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert_eq!(parse(note), Some(Request::Notification));
    }

    #[test]
    fn an_unknown_method_with_an_id_is_answered_not_ignored() {
        let f = r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#;
        match parse(f) {
            Some(Request::Unknown { method, .. }) => assert_eq!(method, "resources/list"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn tool_declarations_use_bare_names_not_harness_spellings() {
        let t = tools();
        let names: Vec<&str> = t
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["spawn", "report"]);
        let s = t.to_string();
        assert!(
            !s.contains("mcp__marion"),
            "the prefix is applied by the harness; baking it in would double it"
        );
    }

    #[test]
    fn report_says_the_final_message_is_not_the_return_value() {
        let t = tools();
        let desc = t[1]["description"].as_str().unwrap();
        assert!(
            desc.contains("NOT the return value"),
            "this is the confusion 7.6 exists for"
        );
    }

    #[test]
    fn initialize_answers_with_a_version_we_support() {
        let v = initialize_result(&json!(0));
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "marion");
    }

    // ---- §6.1/§7.6: a child's failure is as legible as marion's own -------------------------

    /// A contract built the way the real path builds it, so the tests below read the *actual*
    /// layered description (`spawn::build_contract`), not a hand-written imitation of it.
    fn ran(outcome: crate::spawn::ChildOutcome) -> TaskContract {
        use marion_core::contract::*;
        use marion_core::encoding::{Duration, SystemTime};
        crate::spawn::build_contract(
            TaskId("019fbf94-53c8-7c60-9f4c-12695a5e79fe".into()),
            AgentId("019fbf94-0000-7000-8000-000000000001".into()),
            RepoIdentity {
                git_common_dir: "/repo/.git".into(),
                head_branch: Some("main".into()),
            },
            Oid("a".repeat(40)),
            Workspace::Worktree {
                path: "/tmp/wt".into(),
                branch: "marion/t1".into(),
            },
            "add a flag",
            &["tests pass".to_string()],
            &[Glob("**".into())],
            &[Glob("src/**".into())],
            Duration::from_secs(900),
            SystemTime::from_unix_millis(1_785_625_628_619),
            &outcome,
            vec![],
            None,
            vec![],
        )
    }

    fn text(v: &Value) -> String {
        v["result"]["content"][0]["text"]
            .as_str()
            .expect("a tool result carries text")
            .to_string()
    }

    fn first_line(v: &Value) -> String {
        text(v).lines().next().unwrap_or_default().to_string()
    }

    /// The measured shape from S13: an opencode-style failure whose whole diagnosis is in-stream,
    /// with an empty stderr, exiting non-zero.
    fn failed() -> crate::spawn::ChildOutcome {
        crate::spawn::ChildOutcome {
            narrative: Some("tried".into()),
            failure: Some("This model is no longer available to new users.".into()),
            exit_code: Some(1),
            ..Default::default()
        }
    }

    /// **The point of the whole change.** Each failing status comes back with a plain first line
    /// that names what happened, and where the harness said something, quotes it.
    #[test]
    fn a_failed_child_says_so_on_its_first_line_in_the_harnesss_own_words() {
        let v = spawn_result(&json!(2), "codex-impl", Ok(ran(failed())));
        assert_eq!(
            first_line(&v),
            "marion: the codex child failed — child exited with code 1; the child's stream \
             reported: This model is no longer available to new users."
        );
        assert_eq!(v["result"]["isError"], true, "S9: error-shaped, not fatal");
        assert!(
            text(&v).contains("\"status\": \"Failed\""),
            "the contract is still there underneath; it just stopped being the message"
        );
        // The defect this closes, stated as an assertion. Before, both of these opened with `{`
        // and both carried `isError: false`, so telling them apart meant finding `"status"`
        // somewhere inside a kilobyte of serialized JSON.
        let good = spawn_result(
            &json!(2),
            "codex-impl",
            Ok(ran(crate::spawn::ChildOutcome {
                narrative: Some("did the work".into()),
                exit_code: Some(0),
                ..Default::default()
            })),
        );
        assert_ne!(
            first_line(&v),
            first_line(&good),
            "a failed child must not open the same way a successful one does"
        );
        assert_ne!(v["result"]["isError"], good["result"]["isError"]);
    }

    #[test]
    fn a_timed_out_child_says_it_timed_out_not_that_it_crashed() {
        let v = spawn_result(
            &json!(2),
            "codex-impl",
            Ok(ran(crate::spawn::ChildOutcome {
                timed_out: true,
                ..Default::default()
            })),
        );
        assert_eq!(
            first_line(&v),
            "marion: the codex child timed out — child exceeded its timeout and its process group \
             was killed"
        );
        assert_eq!(v["result"]["isError"], true);
    }

    /// §7.6: a child that never called `report` is a *distinct* outcome, not a crash. Saying
    /// "failed" here would invite the parent to conclude the work did not happen, when what marion
    /// actually knows is only that no answer was returned.
    #[test]
    fn an_unreported_child_is_named_as_never_having_reported_not_as_a_failure() {
        let v = spawn_result(
            &json!(2),
            "codex-impl",
            Ok(ran(crate::spawn::ChildOutcome {
                exit_code: Some(0),
                ..Default::default()
            })),
        );
        let line = first_line(&v);
        assert_eq!(
            line,
            "marion: the codex child never called report, so it returned no answer — child exited \
             with code 0"
        );
        assert!(
            !line.contains("failed"),
            "never silently promote, and never silently demote either: {line}"
        );
        assert_eq!(v["result"]["isError"], true);
    }

    /// The success path is not merely similar — it is the same bytes it was before, so a
    /// `cross_product` or `harness_matrix` cell that reads a returned contract cannot notice this
    /// change happened.
    #[test]
    fn a_successful_child_is_byte_identical_to_what_it_always_returned() {
        let c = ran(crate::spawn::ChildOutcome {
            narrative: Some("did the work".into()),
            exit_code: Some(0),
            ..Default::default()
        });
        let before = tool_result(&json!(2), &serde_json::to_string_pretty(&c).unwrap(), false);
        assert_eq!(spawn_result(&json!(2), "codex-impl", Ok(c)), before);
        assert_eq!(before["result"]["isError"], false);
    }

    /// A launch failure and a run failure are the same news, so they open the same way — and the
    /// typed diagnosis survives into the sentence rather than being flattened to "spawn failed".
    #[test]
    fn a_launch_failure_and_a_run_failure_read_alike() {
        let launch = spawn_result(
            &json!(2),
            "codex-impl",
            Err(crate::spawn::SpawnError::Gate(
                marion_core::agent_type::SpawnGateError::DepthExceeded {
                    child_depth: 4,
                    max_depth: 3,
                },
            )),
        );
        let run = spawn_result(&json!(2), "codex-impl", Ok(ran(failed())));
        for v in [&launch, &run] {
            let l = first_line(v);
            assert!(l.starts_with("marion: the "), "{l}");
            assert!(l.contains(" child "), "{l}");
            assert!(
                l.contains(" — "),
                "the verb and the diagnosis are split: {l}"
            );
            assert_eq!(v["result"]["isError"], true);
        }
        assert_eq!(
            first_line(&launch),
            "marion: the codex-impl child could not be launched — spawn refused (§6.1 step 2): \
             spawn would create a node at depth 4, past max_depth 3",
            "the bound and the value it broke are still in the sentence"
        );
    }

    /// The contract stops being the *message*; it does not stop being the record. What rides
    /// beneath the failure line is the returned contract in full, unaltered — and since this module
    /// only ever reads the value `run_spawn` hands back, and `persist_then_cap` writes the disk copy
    /// before capping and before returning, nothing here can reach what was persisted. (The disk
    /// copy itself is asserted by `cross_product`, `harness_matrix`, `m1_hop`, `depth_gate` and
    /// `journal_wiring`, which read `contracts/<task_id>.json` back.)
    #[test]
    fn the_contract_beneath_the_failure_line_is_the_whole_unaltered_record() {
        let c = ran(failed());
        let expected = serde_json::to_string_pretty(&c).unwrap();
        let body = text(&spawn_result(&json!(2), "codex-impl", Ok(c.clone())));
        let (line, rest) = body.split_once("\n\n").expect("a line, then the record");
        assert!(line.starts_with("marion: the codex child failed"));
        assert_eq!(rest, expected);
        // And it is still a document, not prose with JSON in it: the prefix lives outside.
        let back: TaskContract =
            serde_json::from_str(rest).expect("what rides beneath still parses as a contract");
        assert_eq!(back.completion.unwrap().exit.description, {
            let e = c.completion.unwrap().exit.description;
            assert!(line.ends_with(&e), "the line quotes it: {line}");
            e
        });
    }

    /// §6.7's arithmetic is not disturbed: the prefix is bounded whatever the harness said, so no
    /// diagnosis can push a tool result past the size at which a harness stubs it out.
    #[test]
    fn a_very_long_harness_error_cannot_grow_the_prefix_without_bound() {
        let v = spawn_result(
            &json!(2),
            "codex-impl",
            Ok(ran(crate::spawn::ChildOutcome {
                failure: Some("é".repeat(40_000)),
                exit_code: Some(1),
                ..Default::default()
            })),
        );
        let line = first_line(&v);
        assert!(line.len() <= SUMMARY_CAP + '…'.len_utf8(), "{}", line.len());
        assert!(line.ends_with('…'), "the elision is marked, not silent");
        assert!(line.starts_with("marion: the codex child failed — "));

        let launch = first_line(&spawn_result(
            &json!(2),
            "codex-impl",
            Err(crate::spawn::SpawnError::Git("command", "x".repeat(40_000))),
        ));
        assert!(launch.len() <= SUMMARY_CAP + '…'.len_utf8());
    }

    #[test]
    fn handling_is_idempotent_across_repeated_startup() {
        // S6: codex issues two full initialize + tools/list sequences per exec run.
        let a = initialize_result(&json!(0));
        let b = initialize_result(&json!(0));
        assert_eq!(a, b);
        assert_eq!(tools_list_result(&json!(1)), tools_list_result(&json!(1)));
    }
}
