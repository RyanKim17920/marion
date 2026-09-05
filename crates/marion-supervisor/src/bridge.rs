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
use crate::tool::{ContentBlock, ToolOutcome};

/// Every MCP revision marion will answer `initialize` with, **oldest first**.
///
/// # What claiming a version means here, and why marion can claim all four
///
/// marion's server surface is `initialize`, `tools/list`, `tools/call`, and text content blocks
/// with an `isError` flag. That subset is byte-identical across all four revisions listed below:
/// nothing marion sends or reads was added, removed or reshaped between `2024-11-05` and
/// `2025-11-25`. The revisions did change plenty — `2025-03-26` added audio content and tool
/// annotations, `2025-06-18` added `structuredContent`, `outputSchema` and elicitation and removed
/// JSON-RPC batching, `2025-11-25` continued on top — but marion emits none of it. Claiming a
/// version is a promise not to send a frame that revision cannot parse, and marion's frames are in
/// the intersection, so the promise holds for every entry here.
///
/// This is why the list is not "the newest one". Bumping a single constant would trade one wrong
/// answer for another: it would tell a `2024-11-05` client that marion speaks a revision that
/// client has never heard of, when in fact marion's frames would have been fine for it.
///
/// # The one thing `2024-11-05` claims that marion does not implement
///
/// Batching. `2024-11-05` and `2025-03-26` permit a client to send a JSON array of requests;
/// `2025-06-18` removed it. marion reads one frame per line and would answer an array with
/// [`invalid_request`] rather than a batch of results. This is recorded rather than fixed: no
/// measured client has ever sent one (s6, s13 and s16 are all one-frame-per-line), the two
/// revisions that allow batching only ever *permitted* it, and implementing a code path nothing
/// exercises would be a second untested surface rather than a fix. If a client ever does batch, it
/// gets a named error and not silence, which is the property that actually matters.
///
/// # Review trigger — adding a version
///
/// Before adding an entry, diff the new revision against the newest one already listed and confirm
/// each of these, because each is something marion actually puts on the wire:
///
/// 1. the `initialize` **result** shape — `protocolVersion`, `capabilities`, `serverInfo`;
/// 2. the `tools/list` **tool** shape — `name`, `description`, `inputSchema`;
/// 3. the `tools/call` **result** shape — the `content` array, the `text` block, and `isError`;
/// 4. whether id-less frames are still notifications and still forbidden a reply;
/// 5. whether the JSON-RPC error codes marion emits ([`method_not_found`], [`parse_error`],
///    [`invalid_request`], [`not_initialized`]) still mean what they mean here.
///
/// If any of those moved, the version does **not** go in this list until the code moves with it.
/// Then add a fixture under `tests/fixtures/protocol/` — one file per claimed version, named for
/// it — and it will be picked up automatically by `tests/mcp_conformance.rs`'s
/// `every_claimed_protocol_version_has_a_fixture_and_is_echoed`, which fails if a claimed version
/// has no fixture or a fixture names no claimed version.
///
/// # The coming break
///
/// `2026-07-28` is deliberately not here and cannot be added by listing it: it removes the
/// `initialize` handshake this constant is consulted from. See §11 item 32 of the design document.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// The revision marion answers a client that offered nothing marion can place.
///
/// The **oldest** claimed version, not the newest. A client whose offer marion cannot place is
/// either older than anything here or too broken to state one, and `2024-11-05` is the only
/// revision in this list a real harness has been *measured* accepting after offering something
/// else — codex 0.146.0 offered `2025-06-18` and took `2024-11-05` five times over in
/// `tests/fixtures/s6/mcp-server-frames.jsonl`.
pub const PROTOCOL_VERSION_FLOOR: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// The revision marion answers `initialize` with, given what the client offered.
///
/// **One rule: the newest version marion supports that is not newer than the client's offer.**
/// MCP requires the server to echo the client's version when it supports it and otherwise to
/// answer one it does support; a single downgrade-to-nearest covers both, and two more cases
/// besides:
///
/// | the client offers | it gets | because |
/// |---|---|---|
/// | `2025-06-18` (claimed) | `2025-06-18` | it is in the list, so the rule returns it — this *is* the echo |
/// | `2025-04-01` (between) | `2025-03-26` | the newest marion has that the client, being newer, can still read |
/// | `2026-07-28` (newer than all) | `2025-11-25` | marion's ceiling; a newer client can speak down |
/// | `2019-01-01`, absent, malformed | `2024-11-05` | nothing is `<=` it, so [`PROTOCOL_VERSION_FLOOR`] |
///
/// Comparison is lexicographic, which is exactly date order for `YYYY-MM-DD` — the only shape MCP
/// revisions have ever taken. A revision that broke that shape would sort somewhere arbitrary, so
/// the review trigger on [`SUPPORTED_PROTOCOL_VERSIONS`] is where that gets caught, not here.
///
/// Note what this function does **not** do: consult a default. There is no answer that ignores the
/// client's offer, which is the whole defect it exists to close.
pub fn negotiate_protocol_version(offer: Option<&str>) -> &'static str {
    let Some(offer) = offer else {
        return PROTOCOL_VERSION_FLOOR;
    };
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .rev()
        .find(|v| **v <= offer)
        .copied()
        .unwrap_or(PROTOCOL_VERSION_FLOOR)
}

/// A decoded inbound request.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Initialize {
        id: Value,
        /// Exactly what the client put in `params.protocolVersion`, un-normalised — `None` when it
        /// sent no version at all, or sent one that was not a string.
        /// [`negotiate_protocol_version`] is the only thing that interprets it, so the two cases it
        /// cannot tell apart are the two cases it answers identically.
        offered_version: Option<String>,
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

/// Why a line could not be decoded into a [`Request`].
///
/// All three used to be the same `None`, and that `None` was dropped with a bare `continue` — so a
/// client that sent marion a broken frame got **silence**, and could wait on it forever. These
/// exist so each reachable way of being undecodable gets the JSON-RPC code that names it.
#[derive(Debug, Clone, PartialEq)]
pub enum Undecodable {
    /// Not JSON. JSON-RPC `-32700`, answered with a `null` id, because there is no parsed frame to
    /// take an id *from* — that is precisely the case `-32700` exists for.
    NotJson,
    /// JSON, but not a frame marion can route: no usable `method`. JSON-RPC `-32600`.
    NotAFrame { id: Value },
    /// A `result`/`error` frame: an *answer*, and marion asks the client nothing, so there is
    /// nothing this could be an answer to. Dropped without a reply, because replying to a reply is
    /// how two peers loop forever. This is the one silent drop left, and it is silent by rule.
    StrayResponse,
}

/// Decode one line of the stdio stream.
///
/// # Why there is no `starts_with("notifications/")` here any more
///
/// There was, and it was wrong in the way [`marion_proto::envelope`]'s own comment describes for
/// its own protocol: *"a prefix or `starts_with` test would route `node/pty-write` into the
/// outbound table"*. The same codebase was rigorous on one surface and loose on the other.
///
/// It was also **unnecessary**, which is why the fix is a deletion rather than a table. JSON-RPC
/// already decides this: a frame with no `id` is a notification and MUST NOT be answered, whatever
/// it is called; a frame *with* an `id` is a request, and an unrecognised one is `-32601`. The
/// prefix arm only ever changed the answer for a frame named `notifications/…` that carried an id
/// — a client bug, which the arm silently swallowed instead of naming. Dropping the arm gives that
/// case `-32601` and leaves every id-less frame exactly where it was, which is what keeps s6's
/// `notifications/initialized` unanswered.
pub fn parse(line: &str) -> Result<Request, Undecodable> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Err(Undecodable::NotJson);
    };
    let id = v.get("id").cloned();
    let Some(method) = v.get("method").and_then(Value::as_str) else {
        // An answer to nothing, versus a frame that is neither call nor answer.
        if v.get("result").is_some() || v.get("error").is_some() {
            return Err(Undecodable::StrayResponse);
        }
        return Err(Undecodable::NotAFrame {
            id: id.unwrap_or(Value::Null),
        });
    };
    Ok(match (method, id) {
        ("initialize", Some(id)) => Request::Initialize {
            id,
            offered_version: v
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
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
/// **`report` on a root is declared and refused**, which is grounds 1 applied to `report` rather
/// than an exception to it. §7.6 and §5.4 both say a root has no contract and may not `report`; the
/// verb stays in this list so the refusal can be a *sentence* ([`REPORT_ON_A_ROOT`], answered by
/// `main::handle_tool_call`) instead of an absence the root would read as "marion has no `report`".
/// Until that refusal existed the bridge answered `report recorded`, `isError: false`, to a root —
/// a receipt for a payload nothing stages, since there is no contract to stage it into.
pub fn tools() -> Value {
    json!([
        {
            "name": "spawn",
            "description": "Delegate a task to a child agent. By default this blocks until the \
                            child reaches a terminal state and returns its completed task \
                            contract. With `background: true` it returns immediately with a \
                            handle and the child runs while you keep working; call `wait` with \
                            that handle's task_id to collect the contract.",
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
            // **Declared because `spawn` now hands out a handle**, and §5.4 is explicit that the
            // handle's holder *"must be able to `wait` a node that may already have exited —
            // denying that would make the handle useless."* §7.6's worked example is blunter: a
            // handle with no primitive to resolve it is the anti-pattern marion exists to delete,
            // and shipping one would be `verification`'s accept-and-ignore wearing a handle's
            // clothes.
            //
            // It also closes a gap that predates backgrounding: `root::ROOT_VERBS` has
            // permitted `wait` since M1 (§9 records the allowlist being wider than the declared
            // surface as "not a bug", since an allowlist entry for an undeclared tool is inert).
            // The entry stops being inert here.
            //
            // **Scoped to this bridge's own children, which is *narrower* than §5.4's rule.**
            // §5.4 permits `wait` against *descendants*; this bridge instance serves one node and
            // its table holds that node's **direct children**, so a grandchild — legitimately
            // waitable under §5.4 — is answered `Unknown` here. That gap is marion's, not the
            // caller's, and `wait_unknown` says so rather than implying the lookup was exhaustive.
            //
            // **Step 5 did not close it, and it is worth saying why not.** The children are the
            // supervisor's now, so the *tree* is in one place for the first time — but a handle is
            // a `task_id`, and none of §2's fifteen methods resolves one to a node.
            // `marion_proto::Method::ALL` is pinned at fifteen and step 5 deliberately adds none,
            // so the pairing is remembered where the supervisor said it: in this process, in the
            // one answer that carried both (`background::Handed`). Closing the grandchild gap needs
            // that lookup on the wire, which is a proto decision and not this file's.
            //
            // There is deliberately no timeout parameter, and that is not the same as there being
            // no timeout. A caller-supplied one could disagree with the contract about whether the
            // run had ended, so the bound is derived instead: the child's own `timeout_secs` plus
            // the grace marion adds for the work around a run. Expiry is not
            // a verdict on the child (`wait_still_running`) — it exists because a `wait` that never
            // returns stops this bridge reading *any* later frame, from anyone.
            "name": "wait",
            "description": "Collect a backgrounded child's task contract, blocking until that \
                            child reaches a terminal state. Takes the task_id from the handle \
                            `spawn` returned. Returns immediately if the child has already \
                            finished. If the child is still running well past its own timeout, \
                            this returns saying so instead of blocking forever — the child keeps \
                            running and the handle stays valid, so you can wait on it again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type": "string"}
                },
                "required": ["task_id"]
            }
        },
        {
            // **§5.4's `status`, and the entry in `root::ROOT_VERBS` stops being inert
            // here** — the same move `wait` made, for the same reason and with the same honesty
            // about how narrow it is.
            //
            // **Addressed by `task_id`, not by node id, because a `task_id` is what a caller
            // holds.** A `spawn { background: true }` hands out a handle and nothing else; the
            // `AgentId` behind it is told to this process exactly once, in `agent/spawn`'s answer,
            // and is never shown to the model. A tool taking an `AgentId` would therefore be
            // addressable only by a caller that had guessed one.
            //
            // **Narrower than §5.4, in the same way `wait` is.** §5.4 permits `status` against
            // descendants *or the parent*, plus `allow_peers` siblings. This resolves through the
            // same per-process table `wait` uses, which holds this node's direct children — so a
            // grandchild, a parent and a peer are all outside it. That is marion's limit and
            // [`status_unknown`] says so rather than implying the lookup was exhaustive; closing it
            // needs a `task_id`-to-node lookup on the wire, which none of §2's fifteen methods has
            // and which is a proto decision, not this file's.
            //
            // **It reads the supervisor every time.** `node/get` is a projection out of the
            // registry the supervisor wrote itself, and nothing here caches it. A `status` that
            // answered from the row this bridge stored at spawn would report `Spawning` forever —
            // an answer that is wrong precisely when it is asked for.
            "name": "status",
            "description": "Check what a backgrounded child is doing right now, without blocking. \
                            Takes the task_id from the handle `spawn` returned, and answers with \
                            the child's current state as marion's supervisor holds it. Works after \
                            the child has finished, and after `wait` has already collected it. \
                            This is a poll, not a wait — if what you actually want is the child's \
                            result, call `wait`, which blocks and returns the contract.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": {"type": "string"}
                },
                "required": ["task_id"]
            }
        },
        {
            // **§5.4's `list` — "discovery"** — and the last of the allowlist's four verbs to stop
            // being inert.
            //
            // **It takes no parameters and it is filtered, which is the load-bearing half.** The
            // only one of §2's fifteen methods that enumerates nodes is `tree/subscribe`, and it
            // answers with *every node in the project* — other roots, unrelated subtrees, the whole
            // fleet. Handing that to a child node would be wider than §5.4 authorizes by a long
            // way, so `main::handle_tool_call` keeps only this caller's own subtree. §5.4's set
            // also includes the parent and `allow_peers` siblings; the parent is included, and
            // `allow_peers` is not built at all, so no sibling appears and the answer does not
            // pretend one might.
            //
            // **A caller with no children gets an empty list and is told so in words**, rather than
            // an empty array it has to interpret. §7.6's rule about handles applies to lists too: a
            // result a model must reason about the absence of is a result that gets misread.
            "name": "list",
            "description": "List the child agents you have spawned and what each is doing now. \
                            Takes no arguments. Answers from marion's supervisor, so it reflects \
                            the current state of every child of yours that is still known — \
                            running or finished. Use it when you have lost track of what you \
                            delegated; use `wait` to collect a specific child's result.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
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

/// marion's own name for the child's return path, in the vocabulary [`tools`] declares it under.
///
/// Stated once because two axes reach for it in two spellings: the bridge is called with this name
/// verbatim, and `duplex` sees whatever the harness spells it as — which is the *adapter's* mapping
/// (`HarnessAdapter::marion_tool_name`) applied to this, never a second literal.
pub const REPORT: &str = "report";

/// **§5.4's answer to a `report` from a node that has no contract**, in that table's own terms.
///
/// Written as a sentence rather than a code, for the reason [`tools`] gives about absence: the node
/// has to be able to tell "marion will not" from "marion could not", and to know what to do
/// instead. So it says which rule (§5.4), which fact about this node makes it apply (it is the
/// root, and a root has no contract — §9), and what the verb it wanted actually is (a *child's*
/// `report`, reached by delegating).
pub const REPORT_ON_A_ROOT: &str = "marion: `report` is self only, and only on a node that has a \
     contract (§5.4). This node is the root: it has no contract, so there is nothing a report could \
     be recorded against — a root's result is its own stream and its exit (§9). Delegate the work \
     with `spawn`; the child's `report` is what returns a result to you.";

/// **The one row of §5.4's authorization table marion can decide from the caller's identity alone**,
/// stated once so the two places that enforce it cannot answer differently.
///
/// Both callers are asking the same question about the same node and neither can defer it: the
/// bridge is about to *perform* the verb (`main::handle_tool_call`), and the duplex driver is about
/// to answer a `can_use_tool` for it with nobody to ask (`duplex::run_duplex`). A rule with two
/// implementations would eventually deny on one axis and serve on the other, which is exactly the
/// drift §9 warns about where it blesses the allowlist being wider than the declared surface.
///
/// It takes a depth rather than a "is this a root" flag because `ROOT_DEPTH` is the definition
/// (§3.1: *"counting the root as 0"*), and every other node's depth is derived from it.
pub fn authorization_refusal(depth: u32, verb: &str) -> Option<&'static str> {
    (verb == REPORT && depth == crate::depth::ROOT_DEPTH).then_some(REPORT_ON_A_ROOT)
}

/// Answer `initialize`, **with a version chosen from what the client offered**.
///
/// `offered` is the client's `params.protocolVersion`. It is a parameter and not a default because
/// the defect this replaced was exactly that: the reply was a constant, and the offer was never
/// read. See [`negotiate_protocol_version`] for the rule.
pub fn initialize_result(id: &Value, offered: Option<&str>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "protocolVersion": negotiate_protocol_version(offered),
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

/// Wrap a [`ToolOutcome`] as the JSON-RPC response to the call with this id.
///
/// **The one place a tool result acquires a request id**, and the whole of what an SDK would
/// replace on this side of the seam. See [`crate::tool`] for the boundary this belongs to and for
/// why `handle_tool_call` does not return through it *yet*.
pub fn tool_outcome_result(id: &Value, outcome: &ToolOutcome) -> Value {
    let content: Vec<Value> = outcome
        .content
        .iter()
        .map(|ContentBlock::Text(t)| json!({"type": "text", "text": t}))
        .collect();
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": content, "isError": outcome.is_error}})
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
        // **The verb comes from the error, not from this line.** Every refusal that predates §11
        // item 28 step 5 is a launch that did not happen, and "could not be launched" is exactly
        // right for it. The ones the socket path added are not all of that shape — a child that
        // ran, and whose contract marion could then not deliver, did real work — and telling a
        // parent its child never started would be the same class of false report as the rest of
        // this module deletes. See [`SpawnError::verb`].
        Err(e) => tool_result(
            id,
            &bounded(&format!(
                "marion: the {agent_type} child {} — {e}",
                e.verb()
            )),
            true,
        ),
    }
}

/// **What a root's `spawn` or `wait` comes back as** — the node ended, and §9 gives it no contract
/// to return.
///
/// This is the shape [`spawn_result`] cannot take, and the reason it is a second function rather
/// than a fourth arm there. Every one of that function's three shapes is built around a
/// `TaskContract`: two print one and the third explains why there is none *and treats that as the
/// failure*. A root has none and has not failed. Routing a successful root through
/// `SpawnError::NoContract` would tell a top-level client that marion lost its answer on every run
/// that went perfectly — the accept-and-blame shape, inverted.
///
/// **The exit numbers ride along and the answer does not pretend to be one.** §9 is explicit that a
/// root has no `TaskContract`, so there is no `result` field anywhere for marion to invent; what a
/// caller gets is the terminal status marion observed, marion's own description of it, and the path
/// to the node's `events.jsonl`, which *is* the record of what the node did. Pointing at the file is
/// the honest end of this sentence: it is what a person reads, it is what `marion attach` replays,
/// and it is a place a caller can actually go — unlike a summary this function would have to make up
/// from a stream it never read.
///
/// `isError` follows [`failure_line`]'s rule, for the same measured reason: `Ok` is `false`,
/// everything else is `true`, because S9 recorded that a harness renders an error-shaped result
/// verbatim and continues the turn. A root that timed out reported as success is the silent-failure
/// shape §12 names.
pub fn root_result(
    id: &Value,
    agent_type: &str,
    agent_id: &marion_core::contract::AgentId,
    status: ExitStatus,
    exit: &marion_core::contract::ProcessExit,
    events: &std::path::Path,
) -> Value {
    let head = match status {
        ExitStatus::Ok => format!("marion: the {agent_type} root finished"),
        ExitStatus::Unreported => {
            format!("marion: the {agent_type} root ended without reporting an outcome")
        }
        ExitStatus::TimedOut => format!("marion: the {agent_type} root timed out"),
        ExitStatus::Cancelled => format!("marion: the {agent_type} root was cancelled"),
        ExitStatus::Killed => format!("marion: the {agent_type} root was killed"),
        ExitStatus::Failed => format!("marion: the {agent_type} root failed"),
    };
    tool_result(
        id,
        &bounded(&format!(
            "{head} — {desc}. It is node {node}, and it is a **root**: §9 gives a root no task \
             contract, so there is no result document to return and marion is not withholding one. \
             What it did is its own event stream, at {events}.",
            desc = exit.description,
            node = agent_id.0,
            events = events.display(),
        )),
        !matches!(status, ExitStatus::Ok),
    )
}

/// **What a backgrounded `spawn` hands back**, written as a sentence for the reason
/// [`REPORT_ON_A_ROOT`] is: the caller is a language model, and a bare identifier teaches it
/// nothing about what to do next.
///
/// §7.6's worked example is the specification for this string. It records a Codex integration that
/// returned `task-ms9ifb08-bkcgol` plus "check `/codex:status`" and calls that *correct for its
/// constraints and missing a layer* — the cost being that **the caller is obliged to know it must
/// poll**, and that a handle in the result slot is *"indistinguishable to it from an answer"*. So
/// this says three things in order: that no result is here yet, what the child is, and the one verb
/// that turns the handle into the contract. It never says "poll" and offers no cadence, because
/// `wait` blocks — marion owns the lifecycle, which is the whole reason it may hand out a handle at
/// all.
/// **The one clause that varies is what the promised `wait` will return**, and it varies because
/// §9 does: a child's `wait` returns a task contract and a root's cannot, since a root has none. A
/// single sentence promising a contract to both would be a receipt written one call before the
/// thing it receipts, for a document that is never going to exist — and a caller that believed it
/// would read a successful root's `wait` as a marion failure. Everything else is deliberately
/// identical, because backgrounding is the same verb later for both kinds.
pub fn background_result(id: &Value, started: &crate::background::Started) -> Value {
    let node = if started.has_contract {
        "child"
    } else {
        "root"
    };
    let returns = if started.has_contract {
        "marion will return its completed task contract"
    } else {
        "marion will return the terminal status it observed — a root has no task contract (§9), so \
         there is no result document to wait for and its event stream is the record"
    };
    tool_result(
        id,
        &format!(
            "marion: the {agent_type} {node} is running in the background — this is a handle, not \
             a result, and it carries no answer yet. Keep working; when you need what the {node} \
             produced, call `wait` with task_id {task_id:?} and {returns}, blocking only if the \
             {node} has not finished yet. You do not need to poll and there is nothing to check in \
             the meantime.",
            agent_type = started.agent_type,
            task_id = started.task_id.0,
        ),
        false,
    )
}

/// **`wait` against an id this bridge process has no row for.**
///
/// A sentence rather than a code, and it has to say **which lookup happened**, because the lookup
/// is narrower than §5.4's rule. §5.4 permits `wait` against *descendants*; the table this searched
/// holds only the caller's **direct children, in this bridge process**. Two things therefore land
/// here that are not caller mistakes at all:
///
/// * a **grandchild or permitted peer** — inside §5.4, outside the table, because that handle was
///   issued by a different bridge instance. The supervisor holds the whole tree since §11 item 28
///   step 5, so the node is reachable; the *handle* is not, because no method on §2's fifteen
///   resolves a `task_id` to a node and step 5 added none.
/// * a handle this bridge really did issue and really has **lost**, because the bridge process was
///   restarted since. The table holds no journal and survives nothing.
///
/// The earlier wording ended *"marion did not search elsewhere and did not lose anything"*. The
/// first half is true and the second is not: on the restart path marion lost the exact handle it
/// was shown. Telling a caller its handle is invalid when marion dropped it is the accept-and-blame
/// shape, so the sentence now names both possibilities and does not claim the search was complete.
pub fn wait_unknown(id: &Value, task_id: &str) -> Value {
    tool_result(
        id,
        &bounded(&format!(
            "marion: this supervisor has no record of task_id {task_id:?}. It looked only at the \
             children it started itself, in this process — that is the whole of the lookup, and it \
             is narrower than what §5.4 permits: a handle for a *grandchild* or for another node's \
             child is not resolvable here even though waiting on it would be legitimate, and a \
             handle marion issued before this supervisor was restarted is gone with the table. So \
             this may be your mistake or it may be marion's, and marion cannot tell which. If the \
             handle came from your own `spawn {{ background: true }}` in this session, it should \
             have \
             been found; treat the child as no longer observable rather than as finished. A \
             synchronous `spawn` returns its contract directly and has no handle to wait on."
        )),
        true,
    )
}

/// **`wait` on a child an earlier `wait` already collected**, told apart by *what* that earlier
/// `wait` received.
///
/// Kept apart from [`wait_unknown`] because the two mistakes have opposite fixes: this caller
/// already holds the answer, and the other is asking about a node this process cannot see.
///
/// **The two variants exist because the old single sentence ended with a claim that is sometimes
/// false**: *"the contract is on disk under that child's agent directory"*. A backgrounded spawn
/// that failed inside its thread — an unknown agent type, a scope refusal, a harness that would not
/// compile, a panicked thread — produces a `SpawnError` and **no contract and no file**. Sending a
/// caller to read a path that was never written is a receipt for something that does not exist,
/// which is the shape §7.6 and this whole bridge exist to delete.
pub fn wait_already_collected(
    id: &Value,
    task_id: &str,
    collected: crate::background::Collected,
) -> Value {
    let tail = match collected {
        crate::background::Collected::Contract => {
            "If you no longer have it, that contract was persisted before it was returned, so it \
             is on disk under that child's agent directory."
        }
        crate::background::Collected::Ended => {
            "That node is a **root**, and §9 gives a root no task contract, so there was never a \
             document to keep — the terminal status you were given the first time is the whole of \
             the answer. Its own event stream is still on disk under that node's agent directory, \
             and that is the record of what it did."
        }
        crate::background::Collected::NoContract => {
            "That child produced no contract — it failed before reaching a terminal state, and the \
             error text you were given the first time is the whole of what marion knows. There is \
             no file to re-read, and waiting again cannot produce one."
        }
    };
    tool_result(
        id,
        &bounded(&format!(
            "marion: the outcome for task_id {task_id:?} was already returned to an earlier \
             `wait`. An outcome is delivered once; marion does not keep a second copy to hand out, \
             and re-waiting would block on a child that has already been collected. {tail}"
        )),
        true,
    )
}

/// **`wait` gave up blocking, and the child is still running.**
///
/// The one answer here that is neither a result nor a mistake, and the sentence has to be careful
/// in both directions at once. It must not read as a failure — the child is fine, its handle is
/// valid, and its own wall clock is still running — and it must not read as a terminal state, which
/// would be the accept-and-ignore shape wearing a timeout's clothes.
///
/// It also has to say what the caller should actually do, because "wait again" is only sometimes
/// right. A child that overran this bound is either doing something slow outside its own clock
/// (`git`, a contract write) or is genuinely stuck, and marion cannot tell which from here — so the
/// sentence offers the choice rather than a cadence, and says plainly that marion has not stopped
/// the child and will not, since stopping it is the child's own `timeout_secs`' job.
pub fn wait_still_running(id: &Value, task_id: &str, agent_type: &str, waited_secs: u64) -> Value {
    tool_result(
        id,
        &bounded(&format!(
            "marion: the {agent_type} child for task_id {task_id:?} has not finished after \
             {waited_secs} s, which is its own wall clock plus the allowance marion adds for the \
             work around a run, so marion stopped blocking rather than hold your turn open \
             indefinitely. Nothing has failed and nothing was cancelled: the child is still \
             running under its own timeout, this handle is still valid, and the slot it occupies \
             is still counted against your concurrency bound. You may keep working and `wait` on \
             it again later. A child that overruns this far is usually blocked on something \
             outside its own clock rather than thinking, so if a second `wait` returns this same \
             answer, treat the node as stuck rather than slow."
        )),
        true,
    )
}

/// **How a node's state is said to a model**, in one place, so `status` and `list` cannot describe
/// the same node in two ways.
///
/// [`marion_proto::model::NodeSummary`] is the whole of what §2 carries about a node, and its
/// `state` is a Rust enum whose `Debug` (`Exited(TimedOut)`, `Blocked(Descendants)`) is not a
/// sentence. What a caller needs from either verb is the same three facts — what it is, where it
/// got to, and whether it is worth waiting for — so they are rendered here rather than at two call
/// sites that would drift.
pub fn node_line(node: &marion_proto::model::NodeSummary) -> String {
    use marion_core::node::NodeState;
    let doing = match &node.state {
        NodeState::Spawning => "starting up".to_string(),
        NodeState::Ready => "started, not yet working".to_string(),
        NodeState::Running => "running".to_string(),
        NodeState::Idle => "idle — it stopped without reporting".to_string(),
        NodeState::Blocked(why) => format!("blocked on {why:?}, waiting for marion"),
        // The one branch a caller acts on differently, so it says the outcome rather than the
        // state's name: a node that is `Exited` has a contract to collect and will not change
        // again, and `Ok` and `TimedOut` are not the same news.
        NodeState::Exited(how) => format!("finished ({how:?})"),
    };
    format!("{} ({}) — {doing}", node.agent_id.0, node.agent_type)
}

/// **`status` for one child**, as a sentence with the handle in it.
///
/// The `task_id` is echoed because the caller addressed the child by that and holds nothing else;
/// an answer naming only an `AgentId` would be about a node the model has never seen a name for.
/// **The closing clause is what a `wait` on this node would actually return**, and it is read off
/// the node rather than assumed: `parent_id.is_none()` is a **root**, and §9 gives a root no
/// `TaskContract`. Promising one here would be the same false receipt
/// [`background_result`] avoids one call earlier — worse, in fact, because `status` is the verb a
/// caller reaches for precisely when it is deciding whether waiting is worth it.
pub fn status_result(id: &Value, task_id: &str, node: &marion_proto::model::NodeSummary) -> Value {
    let returns = if node.parent_id.is_none() {
        "receive the terminal status marion observed — this is a root, and §9 gives a root no task \
         contract, so its own event stream is the record of what it did"
    } else {
        "receive its task contract"
    };
    tool_result(
        id,
        &bounded(&format!(
            "marion: task_id {task_id:?} is {}. This is the state marion's supervisor holds right \
             now, not a cached one. Call `wait` with this task_id to block until it finishes and \
             {returns}.",
            node_line(node)
        )),
        false,
    )
}

/// **`status` against a handle this bridge process has no row for.**
///
/// Kept separate from [`wait_unknown`] rather than shared with it, because the two verbs fail
/// differently in one respect that matters: a `wait` that cannot resolve a handle has *not
/// delivered an outcome*, while a `status` that cannot has merely failed to read one, and the
/// advice at the end therefore differs. Everything before that is the same limit, stated the same
/// way, for the reason [`wait_unknown`] gives at length.
pub fn status_unknown(id: &Value, task_id: &str) -> Value {
    tool_result(
        id,
        &bounded(&format!(
            "marion: this supervisor has no record of task_id {task_id:?}. It looked only at the \
             children it started itself, in this process — that is the whole of the lookup, and it \
             is narrower than what §5.4 permits: a grandchild's handle, or one issued before this \
             supervisor was restarted, is not resolvable here even though reading its state would \
             be legitimate. So this may be your mistake or it may be marion's, and marion cannot \
             tell which. Nothing was read and nothing was changed; `list` shows every child of \
             yours this supervisor can still see."
        )),
        true,
    )
}

/// **`list`'s answer** — the caller's own subtree, already filtered by the caller.
///
/// The empty case is a sentence and not an empty array, deliberately. A model handed `[]` has to
/// infer what the absence means, and the two readings — *"you delegated nothing"* and *"marion lost
/// your children"* — call for opposite next actions. So the empty answer says which one it is.
pub fn list_result(id: &Value, nodes: &[marion_proto::model::NodeSummary]) -> Value {
    if nodes.is_empty() {
        return tool_result(
            id,
            "marion: you have no child agents. Nothing you spawned is still known to this \
             project's supervisor, and nothing failed to appear — this is an empty tree, not a \
             lookup that went wrong. Use `spawn` to delegate a task.",
            false,
        );
    }
    let lines: Vec<String> = nodes
        .iter()
        .map(|n| format!("  {}", node_line(n)))
        .collect();
    tool_result(
        id,
        &bounded(&format!(
            "marion: {} child agent{} of yours, as this project's supervisor holds them right \
             now:\n{}\nCall `wait` with a child's task_id to collect its task contract.",
            nodes.len(),
            if nodes.len() == 1 { "" } else { "s" },
            lines.join("\n")
        )),
        false,
    )
}

pub fn method_not_found(id: &Value, method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id,
           "error": {"code": -32601, "message": format!("no method {method}")}})
}

/// JSON-RPC `-32700`, for a line that was not JSON.
///
/// The id is `null` and cannot be anything else: the frame did not parse, so it has no id to
/// quote. This is the reply that used to be a `continue` — the client got nothing and, if it was
/// waiting on an id it believes it sent, waited forever.
pub fn parse_error(detail: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": Value::Null,
           "error": {"code": -32700,
                     "message": format!("marion could not parse that line: {detail}")}})
}

/// JSON-RPC `-32600`, for JSON that is not a request marion can route.
pub fn invalid_request(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id,
           "error": {"code": -32600, "message":
               "a frame carries a `method`; this one carries none, so it is not a request"}})
}

/// The `initialize`-before-anything-else rule, refused by name.
///
/// `-32002` rather than a `-326xx` code: the JSON-RPC range is for frames that are malformed or
/// unroutable, and this frame is neither — it is well-formed, names a real method, and is merely
/// *early*. `-32002` is the server-defined code LSP and the MCP implementations converged on for
/// exactly this state, so a client that recognises anything will recognise this.
///
/// **This is a rule about `tools/*`, never about `initialize`.** See [`crate::mcp::serve_stdio`]
/// for why a repeated `initialize` must stay legal.
pub fn not_initialized(id: &Value, method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id,
           "error": {"code": -32002, "message": format!(
               "marion has not been initialized: send `initialize` before `{method}`")}})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A root's answer is an error exactly when the root did not finish — never because it has no
    /// contract.**
    ///
    /// §9 gives a root no `TaskContract`, so every top-level `spawn` and `wait` comes back through
    /// [`root_result`] and the absence is the *normal* case. The failure this pins is the obvious
    /// implementation: treating "no contract" as the thing that decides `isError`, which flags every
    /// root — including one that exited `Ok` — as a run marion could not deliver an answer for. That
    /// is the accept-and-blame shape inverted, and it would be invisible in an integration bed whose
    /// harness never exits `Ok` anyway.
    ///
    /// The mapping is `Ok` and the other five, matching [`failure_line`]'s rule for a child, for
    /// S9's measured reason: a harness renders an error-shaped result verbatim and continues the
    /// turn, so `isError` is how marion says "this did not do what you asked" — which is true of a
    /// root that timed out and false of one that finished.
    #[test]
    fn a_roots_result_is_an_error_only_when_the_root_did_not_finish() {
        use marion_core::contract::{AgentId, ProcessExit};

        let node = AgentId("019f-root".into());
        let events = std::path::Path::new("/state/agents/019f-root/events.jsonl");
        let render = |status| {
            root_result(
                &serde_json::json!(1),
                "codex",
                &node,
                status,
                &ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "the process exited".into(),
                },
                events,
            )
        };

        let ok = render(ExitStatus::Ok);
        assert_eq!(
            ok["result"]["isError"],
            serde_json::json!(false),
            "a root that finished is not an error merely because §9 gives it no contract: {ok}"
        );
        let text = ok["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("019f-root") && text.contains("events.jsonl"),
            "and it names the node and the record that does exist: {text}"
        );
        assert!(
            !text.contains("could not") && !text.contains("marion cannot"),
            "and does not describe a successful run as something marion failed to do: {text}"
        );

        for status in [
            ExitStatus::Failed,
            ExitStatus::TimedOut,
            ExitStatus::Cancelled,
            ExitStatus::Killed,
            ExitStatus::Unreported,
        ] {
            let bad = render(status);
            assert_eq!(
                bad["result"]["isError"],
                serde_json::json!(true),
                "a root that ended {status:?} did not do what was asked, and reporting it as a \
                 success is §12's silent failure: {bad}"
            );
            assert!(
                bad["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("events.jsonl"),
                "and every ending points at the record, because that is all a root leaves"
            );
        }
    }

    #[test]
    fn parses_the_frames_a_real_codex_sends() {
        // Verbatim from tests/fixtures/s6/mcp-server-frames.jsonl.
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"codex-mcp-client","title":"Codex","version":"0.146.0"}}}"#;
        assert!(matches!(parse(init), Ok(Request::Initialize { .. })));

        let listed = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"progressToken":0}}}"#;
        assert!(matches!(parse(listed), Ok(Request::ToolsList { .. })));

        let called = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"report","arguments":{"narrative":"s6 probe"}}}"#;
        match parse(called) {
            Ok(Request::ToolsCall {
                name, arguments, ..
            }) => {
                assert_eq!(name, "report");
                assert_eq!(arguments["narrative"], "s6 probe");
            }
            other => panic!("expected a tools/call, got {other:?}"),
        }

        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert_eq!(parse(note), Ok(Request::Notification));
    }

    /// **A `wait` that gave up blocking must not read as a result, and must not read as a
    /// failure.**
    ///
    /// [`wait_still_running`] is the one answer in the `wait` family that reports *neither* an
    /// outcome nor a mistake, and it is the easiest to get wrong in both directions at once. A
    /// caller that reads it as terminal stops waiting on a child that is going to produce a
    /// contract; a caller that reads it as an error may abandon or re-spawn work that is already
    /// running. §7.6's worked example is exactly this hazard — a handle in the result slot being
    /// *"indistinguishable to it from an answer"*.
    ///
    /// The sentence is asserted rather than the shape because the sentence *is* the interface: the
    /// caller is a language model, and there is no code path here for it to branch on.
    ///
    /// This has no end-to-end counterpart on purpose. Reaching this arm through the real bridge
    /// means outlasting a child's whole wall clock plus `background::WAIT_GRACE`, and no test may
    /// hold the suite for minutes to observe a string. `background.rs`'s own unit test covers
    /// *reaching* the state; this covers what is said about it.
    #[test]
    fn a_wait_that_stopped_blocking_says_the_child_is_alive_and_the_handle_still_good() {
        let r = wait_still_running(&json!(1), "task-abc", "codex-impl", 240);
        let text = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert_eq!(
            r["result"]["isError"],
            json!(true),
            "no contract came back, so this is not a result; the alternative is a success-shaped \
             reply carrying no answer"
        );
        assert!(
            text.contains("task-abc") && text.contains("codex-impl"),
            "it names the child it is about — a wait frame carries no agent type: {text}"
        );
        assert!(
            text.contains("still running") && text.contains("still valid"),
            "and says the child is alive and the handle is good, which is what keeps this from \
             reading as terminal: {text}"
        );
        assert!(
            text.contains("240"),
            "and says how long marion actually blocked, so the caller can judge the next move: \
             {text}"
        );
        assert!(
            !text.contains("cancelled") || text.contains("nothing was cancelled"),
            "marion did not stop the child and must not imply it did: {text}"
        );
    }

    /// **A second `wait` points at a file only when there is one.**
    ///
    /// [`wait_already_collected`] ended, unconditionally, *"the contract is on disk under that
    /// child's agent directory"*. For a backgrounded spawn that failed inside its thread there is
    /// no contract and no file, so that was a receipt for something that does not exist. The two
    /// variants are asserted together because the defect is only visible as a contrast.
    #[test]
    fn a_second_wait_promises_a_persisted_contract_only_when_one_was_produced() {
        let with =
            wait_already_collected(&json!(1), "task-ok", crate::background::Collected::Contract);
        let with = with["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            with.contains("on disk"),
            "a delivered contract really was persisted before it was returned: {with}"
        );

        let without = wait_already_collected(
            &json!(2),
            "task-bad",
            crate::background::Collected::NoContract,
        );
        let without = without["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            !without.contains("on disk"),
            "but a spawn that failed wrote no contract, and sending the caller to read one is a \
             receipt for a file that was never written: {without}"
        );
        assert!(
            without.contains("no contract"),
            "and it says so plainly rather than staying silent about it: {without}"
        );
    }

    #[test]
    fn an_unknown_method_with_an_id_is_answered_not_ignored() {
        let f = r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#;
        match parse(f) {
            Ok(Request::Unknown { method, .. }) => assert_eq!(method, "resources/list"),
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
        // `wait` joined the list when `spawn` started handing out handles: §5.4 makes the
        // handle's holder's ability to resolve it a requirement, not an ergonomic. `status` and
        // `list` joined it when they were implemented, which is what made the last two entries of
        // `root::ROOT_VERBS` stop being inert.
        //
        // **The order is pinned, and it is the order a caller meets them in** — delegate, then
        // block on one child, then poll one child, then survey them all, then return your own
        // result. It is not the allowlist's order and does not have to be; the allowlist is a set.
        assert_eq!(names, vec!["spawn", "wait", "status", "list", "report"]);
        let s = t.to_string();
        assert!(
            !s.contains("mcp__marion"),
            "the prefix is applied by the harness; baking it in would double it"
        );
    }

    #[test]
    fn report_says_the_final_message_is_not_the_return_value() {
        let t = tools();
        // Found by name rather than by index: this assertion broke when `wait` was inserted
        // before `report`, and an index is exactly the kind of coupling that turns adding a verb
        // into a false failure about a different verb's wording.
        let desc = t
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "report")
            .expect("report is declared")["description"]
            .as_str()
            .unwrap();
        assert!(
            desc.contains("NOT the return value"),
            "this is the confusion 7.6 exists for"
        );
    }

    /// **The answer is a function of the offer, and no constant satisfies this table.**
    ///
    /// The test this replaced read `assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION)`
    /// against the same constant the code returned. It passed for four spec revisions while marion
    /// never once read the client's `protocolVersion` — it could not have failed, because both
    /// sides of the comparison were the same symbol. That is the vacuous shape this file keeps
    /// finding: an assertion satisfied by something other than the code under test.
    ///
    /// Every row below disagrees with every other row's answer, so **no fixed string passes more
    /// than one of them**. That is the property that makes this a test.
    #[test]
    fn initialize_answers_the_version_the_client_offered_when_it_can() {
        let table = [
            (
                Some("2025-06-18"),
                "2025-06-18",
                "codex 0.146.0's offer, echoed because marion claims it (s6)",
            ),
            (
                Some("2025-11-25"),
                "2025-11-25",
                "opencode 1.17.3's negotiated version, echoed (s13)",
            ),
            (
                Some("2024-11-05"),
                "2024-11-05",
                "the floor, echoed rather than upgraded",
            ),
            (
                Some("2025-03-26"),
                "2025-03-26",
                "the interior entry nothing else would return",
            ),
            (
                Some("2026-07-28"),
                "2025-11-25",
                "newer than marion's ceiling: clamped down, never echoed blind",
            ),
            (
                Some("2025-04-01"),
                "2025-03-26",
                "between two claimed versions: the newest marion has that is not newer",
            ),
            (
                Some("2019-01-01"),
                "2024-11-05",
                "older than anything marion claims: the floor",
            ),
            (None, "2024-11-05", "no offer at all is not an offer"),
            (
                Some("garbage"),
                "2025-11-25",
                "unparseable sorts above the ceiling and clamps to it",
            ),
        ];
        for (offered, expected, why) in table {
            let v = initialize_result(&json!(0), offered);
            assert_eq!(
                v["result"]["protocolVersion"], expected,
                "offered {offered:?}: {why}"
            );
            assert!(
                SUPPORTED_PROTOCOL_VERSIONS
                    .contains(&v["result"]["protocolVersion"].as_str().unwrap()),
                "and whatever it answered is a version marion actually claims"
            );
        }
        assert_eq!(
            initialize_result(&json!(0), None)["result"]["serverInfo"]["name"],
            "marion"
        );
    }

    /// **The five `2024-11-05` answers in s6 were what marion said, not what codex required.**
    ///
    /// s6 captured codex 0.146.0 offering `2025-06-18` and accepting `2024-11-05` five times over.
    /// That is evidence about *codex's* backward compatibility, and it is the reason `2024-11-05`
    /// is the floor rather than something dropped. It is **not** evidence that marion must keep
    /// answering it — after negotiation the same offer gets `2025-06-18`, which is what the client
    /// asked for in the first place.
    ///
    /// This test pins that reading so nobody restores the constant to "keep s6 green": the s6
    /// out-frames are a historical capture, and `tests/fixtures/s6/README.md` now says so.
    #[test]
    fn the_floor_is_the_version_a_real_codex_was_measured_accepting() {
        assert_eq!(PROTOCOL_VERSION_FLOOR, "2024-11-05");
        assert_eq!(
            negotiate_protocol_version(None),
            "2024-11-05",
            "the floor is what an unplaceable offer gets"
        );
        assert_eq!(
            negotiate_protocol_version(Some("2025-06-18")),
            "2025-06-18",
            "but codex's actual offer is now echoed, not answered with the floor"
        );
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
                git_common_dir: Some("/repo/.git".into()),
                head_branch: Some("main".into()),
            },
            Some(Oid("a".repeat(40))),
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
            Some(vec![]),
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
        let a = initialize_result(&json!(0), Some("2025-06-18"));
        let b = initialize_result(&json!(0), Some("2025-06-18"));
        assert_eq!(a, b);
        assert_eq!(tools_list_result(&json!(1)), tools_list_result(&json!(1)));
    }

    // --- the tool seam, measured against the builders above -------------------------------

    use crate::tool::from_wire;

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
            let wire = tool_outcome_result(&json!(7), &outcome);
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
        assert_eq!(from_wire(&method_not_found(&json!(1), "x")), None);
        assert_eq!(from_wire(&parse_error("nope")), None);
        assert_eq!(from_wire(&not_initialized(&json!(1), "tools/call")), None);
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
    /// This is what the seam buys before adoption: the builders here are held to "denotes
    /// exactly one outcome, with an explicit verdict" today, so step 2 of *Adoption* is a refactor
    /// with a net already under it.
    #[test]
    fn todays_result_builders_all_denote_an_outcome_with_an_explicit_verdict() {
        let id = json!(1);
        let frames = [
            ("wait_unknown", wait_unknown(&id, "t-1")),
            (
                "wait_still_running",
                wait_still_running(&id, "t-1", "codex", 30),
            ),
            ("status_unknown", status_unknown(&id, "t-1")),
            ("list_result", list_result(&id, &[])),
            ("tool_result", tool_result(&id, "a sentence", true)),
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
