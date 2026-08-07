//! `marion-supervisor` — the supervisor binary.
//!
//! `mcp`, `serve` and `doctor` are **subcommands of this binary**, not separate ones (§10). The
//! user-facing `marion` command forwards `doctor` here, since the supervisor owns the registry
//! the check reads.
//!
//! `serve` is §10's M2+ row arriving: *"a detached `marion-supervisor`, which `marion` starts on
//! demand"*. It is on **this** binary and not on `marion` because §5.7 starts a supervisor *by a
//! client that finds nothing listening*, so no operator ever needs to type it — and because
//! `bin/marion.rs` refuses every argv[0] but `run`, a refusal that is pinned by a test and that
//! this change deliberately leaves standing. `detach.rs` owns all three of its stages.

use std::io::{BufRead, Write};
use std::time::Duration;

use marion_core::contract::{AgentId, TaskId};
use marion_core::paths::{ProjectDir, state_dir};
use marion_harness::claude_code::NODE_TOKEN_ENV;
use marion_supervisor::root::{AGENT_ID_ENV, DEPTH_ENV, READY_FILE_ENV};
use marion_supervisor::socket::SocketPaths;
use marion_supervisor::spawn::SpawnError;
use marion_supervisor::{background, bridge, courier, detach, run, socket, spawn};

fn usage() -> ! {
    eprintln!("usage: marion-supervisor <mcp|serve|doctor>");
    std::process::exit(2)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("mcp") => run_bridge(),
        Some(detach::SERVE) => {
            let program = std::env::current_exe().unwrap_or_else(|_| "marion-supervisor".into());
            if let Err(e) = detach::run_serve(program, &argv[1..]) {
                // The one place a supervisor stage can speak. Stages 1 and 2 inherit the launcher's
                // stderr, so this reaches the operator who asked for a supervisor; stage 3's stderr
                // is a file beside the socket, so it reaches whoever goes looking afterwards.
                eprintln!("marion-supervisor: {e}");
                std::process::exit(1);
            }
        }
        Some("doctor") => {
            println!("marion doctor: no adapters registered yet (M1 in progress)");
        }
        _ => usage(),
    }
}

/// `spawn` asks this project's supervisor for a child and returns its completed contract — or, with
/// `background: true`, returns a handle immediately; `wait` resolves that handle; `report` stages
/// the child's payload, which the parent's own `spawn` then returns.
///
/// **Since §11 item 28 step 5 nothing here starts a process.** The `spawn` arm dials §2's socket
/// and sends `agent/spawn`; the supervisor owns the child's thread, its `Child`, its pipe and its
/// pid, and this process is a courier for the request and the answer. See [`courier`] for the three
/// decisions that shape it — no fallback, a derived socket path, and a synchronous `spawn` that is
/// a client-side composition rather than a sixteenth method.
///
/// **`bg` is threaded through rather than being a `static`** so the table's lifetime is the
/// bridge's, and so the unit tests below can each hold their own. A process-wide table would make
/// one test's answers depend on which other tests had run — the exact defect class this repo keeps
/// finding, one level down.
fn handle_tool_call(
    bg: &background::Background,
    id: &serde_json::Value,
    name: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
    match name {
        "report" => match report_refusal(std::env::var(DEPTH_ENV).ok()) {
            // §5.4, at the execution point. See [`report_refusal`].
            Some(msg) => bridge::tool_result(id, &msg, true),
            // Staged, not delivered: the contract is written at the node's terminal transition.
            None => bridge::tool_result(id, "report recorded", false),
        },
        "spawn" => {
            // **First, and before the environment is even consulted**, because the answer does not
            // depend on it. Routed through `spawn_result` so a refusal reads like every other one
            // a `spawn` can return.
            if let Some(e) = unimplemented_parameter(args) {
                return bridge::spawn_result(
                    id,
                    args["agent_type"].as_str().unwrap_or("codex-impl"),
                    Err(e),
                );
            }
            let agent_type = args["agent_type"]
                .as_str()
                .unwrap_or("codex-impl")
                .to_string();
            // Two refusals before anything is sent, and both are about a bridge that was started
            // wrong rather than about the call: one cannot work out *which supervisor* to ask, the
            // other cannot say *who is asking*. Neither is a rule the caller broke, so neither
            // reads like `spawn_result`'s refusals — the fix is in the node's declaration.
            let (sock, project) = match supervisor_paths() {
                Ok(v) => v,
                Err(e) => return bridge::tool_result(id, &e, true),
            };
            let caller = match node_identity() {
                Ok(c) => c,
                Err(e) => return bridge::tool_result(id, &e, true),
            };
            // **Every gated fact is left to the supervisor**, and that is the shape of step 5 rather
            // than a simplification. §6.1 step 2's gates read the caller's agent type, its depth and
            // its live-children count; all three used to be read here — the first two off the
            // declaration marion wrote into this process's environment, the third off a table this
            // process kept. The supervisor derives all three from the registry it wrote itself, so a
            // caller can no longer state any of them, and the `MARION_DEPTH`-unreadable refusal that
            // guarded the spawn path is gone with the field it guarded. `SpawnCaller` states who,
            // and proves it; nothing else.
            let params = marion_proto::params::AgentSpawnParams {
                agent_type: agent_type.clone(),
                prompt: args["prompt"].as_str().unwrap_or_default().to_string(),
                caller: Some(caller),
                // Forbidden with a caller, by name: the supervisor already knows which tree this
                // node lives in, and a caller that states it is a caller that can lie about it.
                repo: None,
                acceptance_criteria: string_list(&args["acceptance_criteria"]),
                writable_scope: string_list(&args["writable_scope"]),
                // **Sent as the caller stated it, absent and all.** The wire carries an `Option` so
                // that the supervisor performs the one resolution (`handler`'s own default, then
                // `effective_timeout`'s clamp); a number invented here would be a second source of
                // truth for §3.1's key, and the node could run under a bound this tool never named.
                timeout_secs: args["timeout_secs"].as_u64(),
                // Absent is not empty: `None` falls back to the agent type's own `model` key
                // (§3.1), which is what makes a `gemini` or `opencode` spawn launchable without
                // the parent having to know which harness needs a model and in what spelling.
                model: args["model"].as_str().map(str::to_string),
                // Root-only (§9): a child's writes are judged against the worktree marion made it,
                // so there is no snapshot of anybody's checkout here to decline.
                no_change_record: None,
                pane: None,
            };
            // **The dial. There is no other branch.** A supervisor that does not answer is a
            // refusal in marion's own voice — see [`SpawnError::SupervisorUnreachable`] and
            // [`courier`] for why an in-process fallback is the one thing this must not have.
            let spawned = match courier::spawn(sock.socket(), params) {
                Ok(s) => s,
                Err(e) => return bridge::spawn_result(id, &agent_type, Err(e)),
            };
            let Some(task_id) = spawned.task_id else {
                // Unreachable through a supervisor of this version — `agent/spawn` names the
                // contract file for every spawn that has a caller — and answered rather than
                // panicked, because a child really was started and the parent needs to know that
                // much even when marion cannot say where its answer will land.
                return bridge::spawn_result(
                    id,
                    &agent_type,
                    Err(SpawnError::NoContract {
                        path: project.agent(&spawned.agent_id).contracts_dir(),
                        why: "this project's supervisor started the child without naming the \
                              contract file it will write (`agent/spawn` answers with a `task_id` \
                              for every spawn that has a caller), so marion cannot tell which run \
                              to read back"
                            .into(),
                    }),
                );
            };
            let bound = wait_bound(args["timeout_secs"].as_u64());
            // **§5.4's `background`, read for its value.** Absent and `false` both mean "block".
            // `true` records the pairing the answer just carried and hands back a handle in the
            // same frame; the child is already running either way, because `agent/spawn` answers
            // when the process exists. The two paths now differ in *when the caller is told*, and
            // in nothing else — before step 5 they also differed in which thread ran the child.
            //
            // §6.1 step 2's gates were evaluated here on the background path, because a refusal
            // reaching the caller only through a later `wait` would have answered *"the child is
            // running"* about a spawn marion had already decided to refuse. That cannot happen now:
            // the supervisor evaluates the gates before it answers *this* call, so a refused spawn
            // is refused in the frame that asked for it on both paths, by construction rather than
            // by a second call site.
            if args["background"].as_bool() == Some(true) {
                let started = bg.hand_out(task_id, spawned.agent_id, agent_type, bound);
                return bridge::background_result(id, &started);
            }
            // The blocking half: read the node's own stream until it ends, then read the contract
            // the supervisor wrote before that bookend. A child that ran and failed and a spawn
            // that never launched are the same news to the parent, and `bridge::spawn_result` is
            // where that is decided, in one place, so both read alike.
            bridge::spawn_result(
                id,
                &agent_type,
                match courier::await_contract(
                    sock.socket(),
                    &project,
                    &spawned.agent_id,
                    &task_id,
                    bound,
                ) {
                    Ok(courier::Delivered::Contract(c)) => Ok(*c),
                    // The bridge stopped holding this caller's turn; the child did not stop. Said
                    // as its own sentence rather than as a failure, and the caller is pointed at
                    // the handle-shaped way to ask again.
                    Ok(courier::Delivered::StillRunning) => {
                        Err(SpawnError::OutlivedTheWait(bound.as_secs()))
                    }
                    Err(e) => Err(e),
                },
            )
        }
        // **The handle's resolving verb** (§5.4). Deliberately the same `bridge::spawn_result`
        // that a synchronous `spawn` returns through: the two paths differ in *when* the caller
        // gets the contract and in nothing else, so a model that has learnt to read one reads the
        // other. Answering a `wait` in a second shape would make backgrounding a different verb
        // rather than the same verb, later.
        "wait" => {
            let Some(task_id) = args["task_id"].as_str() else {
                return bridge::tool_result(
                    id,
                    "marion: `wait` needs the `task_id` from the handle a `background: true` \
                     spawn returned. Refusing rather than guessing which of your children you \
                     meant — a wait on the wrong child would block on work you were not asking \
                     about.",
                    true,
                );
            };
            // The table answers **which node** — the one fact only `agent/spawn`'s answer carried
            // — and the blocking is the same read the synchronous path does.
            let (agent_id, agent_type, bound) = match bg.resolve(task_id) {
                background::Wait::Pending {
                    agent_id,
                    agent_type,
                    bound,
                } => (agent_id, agent_type, bound),
                background::Wait::Unknown => return bridge::wait_unknown(id, task_id),
                background::Wait::AlreadyCollected(what) => {
                    return bridge::wait_already_collected(id, task_id, what);
                }
            };
            let (sock, project) = match supervisor_paths() {
                Ok(v) => v,
                Err(e) => return bridge::tool_result(id, &e, true),
            };
            match courier::await_contract(
                sock.socket(),
                &project,
                &agent_id,
                &TaskId(task_id.to_string()),
                bound,
            ) {
                Ok(courier::Delivered::Contract(c)) => {
                    bg.collected(task_id, background::Collected::Contract);
                    // The agent type for the result line comes from the table, not from these
                    // arguments: a `wait` carries no `agent_type`, and inventing one would put a
                    // name in an answer that names the wrong thing.
                    bridge::spawn_result(id, &agent_type, Ok(*c))
                }
                // Deliberately **not** routed through `spawn_result`: every other arm here carries
                // an outcome, and this one carries the absence of an outcome about a child that is
                // still going. Flattening it into the contract-shaped reply would make "still
                // running" indistinguishable from "ran and produced nothing", which is the exact
                // confusion §7.6's worked example is about. The handle is left uncollected, because
                // the child really is still running and its contract really will be written.
                Ok(courier::Delivered::StillRunning) => {
                    bridge::wait_still_running(id, task_id, &agent_type, bound.as_secs())
                }
                Err(e) => {
                    // **Only a terminal outcome burns the handle.** A node that aborted, or whose
                    // contract marion cannot read, has nothing more to give and a second `wait`
                    // must be told so. A supervisor that could not be reached is a different fact
                    // entirely — nothing was learnt about the child — and marking the handle
                    // collected there would turn one unreachable moment into a handle that can
                    // never be resolved again.
                    if matches!(
                        e,
                        SpawnError::NodeAborted(_) | SpawnError::NoContract { .. }
                    ) {
                        bg.collected(task_id, background::Collected::NoContract);
                    }
                    bridge::spawn_result(id, &agent_type, Err(e))
                }
            }
        }
        // **§5.4's `status`: a read of one child, answered from the supervisor every time.**
        //
        // Two lookups and neither may be skipped. The table answers *which node* — the pairing only
        // `agent/spawn`'s answer carried — and `node/get` answers *what that node is doing*, out of
        // the registry the supervisor wrote itself. Answering the second from the first is the
        // failure this verb is most exposed to: `background::Handed` records a child at the instant
        // it was handed out, so a `status` served from it would say `Spawning` about a node that
        // finished an hour ago, and would say it with `isError: false`.
        //
        // Resolved through `node_of` rather than `resolve`, because a handle an earlier `wait`
        // collected still names a node and §5.4 permits `status` against a target in any state.
        "status" => {
            let Some(task_id) = args["task_id"].as_str() else {
                return bridge::tool_result(
                    id,
                    "marion: `status` needs the `task_id` from the handle a `background: true` \
                     spawn returned. Refusing rather than guessing which of your children you \
                     meant — a state reported about the wrong child is worse than no answer, \
                     because nothing in it would look wrong. Call `list` to see every child of \
                     yours and its state.",
                    true,
                );
            };
            let Some((agent_id, _)) = bg.node_of(task_id) else {
                return bridge::status_unknown(id, task_id);
            };
            let (sock, _) = match supervisor_paths() {
                Ok(v) => v,
                Err(e) => return bridge::tool_result(id, &e, true),
            };
            match courier::node_get(sock.socket(), &agent_id) {
                Ok(r) => bridge::status_result(id, task_id, &r.node),
                // The supervisor's own sentence where there is one (`SupervisorRefused` carries a
                // `not_found` that already names the id and how current the registry is), and
                // marion's where the supervisor could not be reached at all. Neither is reworded.
                Err(e) => bridge::tool_result(id, &format!("marion: {e}"), true),
            }
        }
        // **§5.4's `list`: discovery, filtered to what §5.4 authorizes this caller to see.**
        //
        // `tree/subscribe` is the only method of §2's fifteen that enumerates nodes and it answers
        // with the whole project — every root, every unrelated subtree. The filter below is
        // therefore not a convenience: an unfiltered answer would hand a child node the entire
        // fleet, which is wider than §5.4's *"descendants or parent"* by everything else running.
        //
        // The caller's identity is required for that reason and refused when absent, exactly as
        // `spawn` refuses it: a `list` that could not say who is asking could not filter, and a
        // `list` that could not filter must not answer.
        "list" => {
            let caller = match node_identity() {
                Ok(c) => c,
                Err(e) => return bridge::tool_result(id, &e, true),
            };
            let (sock, _) = match supervisor_paths() {
                Ok(v) => v,
                Err(e) => return bridge::tool_result(id, &e, true),
            };
            match courier::tree(sock.socket()) {
                Ok(r) => bridge::list_result(id, &descendants_of(&caller.agent_id, &r.nodes)),
                Err(e) => bridge::tool_result(id, &format!("marion: {e}"), true),
            }
        }
        other => bridge::tool_result(id, &format!("marion: no tool {other}"), true),
    }
}

/// **§5.4's `list` target set, computed over the one tree edge the wire carries.**
///
/// `NodeSummary::parent_id` is the only relation in [`marion_proto::model::NodeSummary`], so the
/// subtree is walked here rather than asked for — no method of §2's fifteen takes a root and
/// answers a subtree, and adding one to save this loop would be a sixteenth method for an
/// arithmetic that fits in a dozen lines.
///
/// **The caller itself is excluded and its descendants are not.** §5.4's row reads *"descendants or
/// parent"*; a caller asking what it delegated is not asking about itself, and including the node
/// doing the asking is how a model comes to `wait` on its own handle. The parent is likewise
/// omitted: §5.4 permits reading it, but `list` answers *"who did I delegate to"*, and a parent in
/// that list is an invitation to address upward that §5.4's `wait` row explicitly refuses.
///
/// Pure and separate from the dispatch arm so the filter is testable as a filter — the same split
/// [`unimplemented_parameter`] keeps, and for the same reason: this is the whole of the
/// authorization boundary for this verb, and a boundary nothing pins is a boundary that drifts.
fn descendants_of(
    caller: &AgentId,
    nodes: &[marion_proto::model::NodeSummary],
) -> Vec<marion_proto::model::NodeSummary> {
    // Breadth-first over `parent_id`, so a grandchild is included and no node is visited twice.
    // A cycle cannot arise from a journal marion wrote — a node's parent is fixed at its
    // `SpawnIntent` — but the `visited` set is what makes that a property of this loop rather than
    // a belief about the input.
    let mut frontier = vec![caller.clone()];
    let mut visited = vec![caller.clone()];
    let mut found = Vec::new();
    while let Some(parent) = frontier.pop() {
        for n in nodes {
            if n.parent_id.as_ref() == Some(&parent) && !visited.contains(&n.agent_id) {
                visited.push(n.agent_id.clone());
                frontier.push(n.agent_id.clone());
                found.push(n.clone());
            }
        }
    }
    found
}

/// **§5.4's `report` row, evaluated where all four harnesses pass through.**
///
/// §5.4: `report` is *"self only, and only on a node that has a contract — rejected on a root."*
/// Three axes could carry that rule and only this one reaches every node:
///
/// * **Availability** — not declaring `report` to a root. `bridge::tools`' own docs reject that on
///   three grounds, and the first is decisive: an absent verb carries no sentence, so the root
///   cannot tell "marion has no `report`" from "I may not report", and nothing anywhere says why.
/// * **Permission** — `root::ROOT_ALLOWED_TOOLS`, which already omits it. That axis is
///   **Claude-Code-only**: it is the one adapter that compiles `allowed_tools` into anything
///   (`claude_code.rs`), so a fix living there is a one-harness fix for a four-harness problem. On
///   codex, gemini and opencode a root's `report` arrived here and was answered `report recorded`,
///   `isError: false` — the payload discarded, the root told it had succeeded, and the run exiting
///   `Ok` having delegated nothing.
/// * **Execution** — here. This is exactly the precedent `run::check_spawn_gates` set for the
///   child's mirrored hole: `spawn` is declared to every child and disallowed for it, and on the
///   `LaunchOnly` harnesses it was simply *served* — real processes, real worktrees, no error
///   anywhere — until the gate moved to the point where the verb is performed.
///
/// **An unreadable depth refuses too — in the other sentence.** It used to serve, on the argument
/// that nothing is created either way and that the refusal would be a claim about this node marion
/// has no grounds to make. The first half is true and the second half is the reason for the second
/// sentence, not for serving: what marion answered instead was `report recorded`, `isError: false`,
/// which is *also* a claim it has no grounds for, and the expensive one — a receipt for a payload
/// nothing stages, after which the node exits `Ok` having returned nothing. That is the same
/// false-success this function was added to delete, still reachable through a declaration written
/// wrong, and the defect class this repository keeps re-finding: a check that reports success by
/// failing to look. An absence is recorded as an absence.
///
/// So there are two refusals, and they must not read alike. [`bridge::REPORT_ON_A_ROOT`] asserts
/// something about the node — *you are the root, and a root has no contract* — and marion may only
/// say it when it knows the depth. The other says what actually happened: the caller's depth could
/// not be established, so no authorization decision was possible, and the fix is in the node's
/// declaration (`root::mcp_config_json`) rather than in the call. A node that gets one has broken a
/// rule; a node that gets the other was started wrong, and only the first is something it can act
/// on.
///
/// **`spawn` no longer reads this key and `report` still does**, which is why the parse below has
/// one caller where it used to have two. §11 item 28 step 5 moved the `spawn` gate to the
/// supervisor, which derives the caller's depth from the `SpawnIntent` it wrote itself rather than
/// believing a declaration — so the whole `MARION_DEPTH`-unreadable refusal class went with it on
/// that path. `report` is decided *here*, on a node with no socket call to make, so it reads the
/// declaration and refuses when it cannot. All four adapters emit `MARION_DEPTH`, swept by
/// `marion_harness::adapter`'s own test, so the only caller that can land in that refusal is a
/// hand-started bridge.
fn report_refusal(depth: Option<String>) -> Option<String> {
    match caller_depth(depth) {
        Ok(depth) => bridge::authorization_refusal(depth, bridge::REPORT).map(str::to_string),
        Err(e) => Some(format!(
            "marion: {e}, so this bridge cannot establish the depth of the node calling it and \
             cannot decide whether §5.4 permits this `report`. Refusing rather than answering that \
             a result was recorded — marion does not know what this node is, and nothing stages a \
             report it cannot attribute. This is a broken launch, not a rule: the key belongs in \
             the node's marion server declaration."
        )),
    }
}

/// **Why `MARION_DEPTH` could not be read**, stated once for both gates that read it.
///
/// One caller today ([`report_refusal`]) and two when it was written: the `spawn` gate read the same
/// key, and two hand-rolled parses of one environment variable with two failure directions is how
/// `report` came to *serve* where `spawn` refused. The type is kept for the surviving reader
/// because the direction is the point — unreadable is `Err`, and the caller decides only what to
/// say about it — and because a `None` that falls out of a parse is how the receipt-for-nothing came
/// back the first time.
enum UnreadableDepth {
    Absent,
    NotADepth(String),
}

impl std::fmt::Display for UnreadableDepth {
    /// The clause both refusals open with, so an operator greps one spelling. It names the key and
    /// quotes the value, because "not set" and "set to something wrong" are different fixes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => write!(f, "{DEPTH_ENV} is not set"),
            Self::NotADepth(raw) => write!(f, "{DEPTH_ENV}={raw:?} is not a depth"),
        }
    }
}

/// §3.1's depth, off the declaration marion wrote, with the empty and the oversized treated as what
/// they are: values that are not a depth. `trim` because whitespace survives an env round trip on
/// some shells and the depth is the number — but an all-whitespace value trims to nothing, which
/// parses as nothing and is an error rather than a zero.
fn caller_depth(depth: Option<String>) -> Result<u32, UnreadableDepth> {
    let raw = depth.ok_or(UnreadableDepth::Absent)?;
    raw.trim()
        .parse()
        .map_err(|_| UnreadableDepth::NotADepth(raw))
}

/// **A `spawn` parameter marion declares and does not implement, if this request carries one.**
///
/// §5.4's schema has eleven keys. `run::SpawnRequest` carries six, `background` is now the
/// seventh, and the remaining four split in two — only one half belongs here:
///
/// * **A different verb performed quietly** — `isolation`, `verification` and
///   `allow_concurrent_writes: true`. Each makes marion do something other than what was asked
///   while answering `isError: false`, which is the §12 accept-and-ignore shape
///   (`default_tools_approval_mode`, `trust: true`). Refused, by name, with the value that broke
///   it; see each [`spawn::SpawnError`] variant for its own reason.
/// * **Simply absent** — `name`. `TaskContract` has no `name` field and no verb addresses a node
///   by one, so dropping it changes no answer any caller receives. Left accepted and recorded in
///   §11 item 23; a refusal would cost callers a working spawn and buy no honesty.
///
/// **`background` left this table** when it was implemented, which is the shape §11 item 23 asked
/// for: *"the natural close is to implement… backgrounding, at which point the three refusals and
/// this item come out together."* It comes out one at a time, and the item records which.
///
/// **`allow_concurrent_writes` entered it in the same change, and only in the `true` direction.**
/// Item 23 called this parameter inverted — `true` honoured accidentally, `false` unhonourable —
/// on the premise that the §6.6 holder check being absent meant concurrent writes were permitted
/// de facto. That premise assumed `isolation` was live. It is not: the refusal below makes
/// `shared-cwd` unreachable, so `false` is delivered by construction and `true` is a request whose
/// subject does not exist. The `SpawnError` variant carries the argument, and §11 item 23 is
/// corrected in the same commit rather than left standing with a justification that is measurably
/// wrong.
///
/// **The permitted values are not refused**, which is the whole point of reading the field rather
/// than rejecting its presence: `isolation: "worktree"`, an empty `verification` and
/// `allow_concurrent_writes: false` all describe exactly what marion does, and an absent key asks
/// for nothing. `background` is now read for its value rather than refused for its presence, by
/// [`handle_tool_call`].
///
/// Pure, and separate from [`handle_tool_call`], so the table above is testable as a table.
fn unimplemented_parameter(args: &serde_json::Value) -> Option<spawn::SpawnError> {
    if args["allow_concurrent_writes"].as_bool() == Some(true) {
        return Some(spawn::SpawnError::ConcurrentWritesUnimplemented);
    }
    match args["isolation"].as_str() {
        None | Some("worktree") => {}
        Some(other) => return Some(spawn::SpawnError::IsolationUnimplemented(other.into())),
    }
    if args["verification"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
    {
        return Some(spawn::SpawnError::VerificationUnimplemented);
    }
    None
}

/// **Which supervisor this bridge talks to, and where the nodes it asks about keep their files.**
///
/// §2: *"the per-child MCP bridge derives this path by the same rule"* — and this is that rule,
/// applied literally. `<state>` comes from §4.3's precedence and the key is
/// `git rev-parse --git-common-dir` over the tree this node lives in, so `marion run`,
/// `detach::ensure_supervisor` and this function compute one path from one derivation. **There is
/// deliberately no environment variable for the socket.** A bridge that could be *told* where to
/// dial is a bridge that can be pointed at another project's supervisor, and the failure would be
/// invisible: a real child, a real contract, in the wrong journal.
///
/// The [`ProjectDir`] comes back with it because the two are one lookup and are spent together: the
/// synchronous `spawn` reads `agents/<agent_id>/contracts/<task_id>.json` under exactly the project
/// whose supervisor answered it, and deriving them separately is how a bridge ends up reading one
/// project's contract after asking another project's supervisor.
///
/// Both refusals name the key, because the fix is in the node's declaration and not in the call —
/// the same reason [`report_refusal`] names `MARION_DEPTH`.
fn supervisor_paths() -> Result<(SocketPaths, ProjectDir), String> {
    let repo = std::env::var("MARION_REPO").map_err(|_| {
        "marion: MARION_REPO is not set, so this bridge cannot work out which project's supervisor \
         to ask for a child (§2 keys a supervisor on the tree's git common directory). Refusing \
         rather than guessing: a spawn sent to another project's supervisor would run, and would be \
         journaled somewhere nobody watching this node will look. This is a broken launch, not a \
         rule — the key belongs in the node's marion server declaration."
            .to_string()
    })?;
    let repo = std::path::PathBuf::from(repo).canonicalize().map_err(|e| {
        format!(
            "marion: MARION_REPO does not resolve to a directory marion can read ({e}), so this \
             bridge cannot derive its project's socket path (§2). Refusing rather than guessing."
        )
    })?;
    let legacy = std::env::var("MARION_STATE").ok();
    let documented = std::env::var("MARION_STATE_DIR").ok();
    let state = state_dir(
        documented.as_deref().or(legacy.as_deref()),
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .ok_or_else(|| {
        "marion: this bridge cannot resolve a state directory (MARION_STATE_DIR, else \
         XDG_STATE_HOME, else HOME), so it can derive neither its project's socket path nor where \
         a child's contract would be written (§4.3). Refusing rather than guessing."
            .to_string()
    })?;
    // §2's key — the git common dir, not the cwd. The bridge is spawned *inside* the node's
    // worktree in some configurations, so this is the call that stops a child from being asked of
    // a supervisor for a project of its own. `marion run` and `root::prepare` make the same one.
    let key = socket::project_root(&repo);
    Ok((
        socket::socket_paths(&state, &key, uid()),
        ProjectDir::new(&state, &key),
    ))
}

/// This process's own uid, which §2's `/tmp` fallback path is keyed on.
fn uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: reads the calling process's real uid and cannot fail.
    unsafe { getuid() }
}

/// **Who this bridge is serving, and its proof** — §5.4's *"per-node capability token bound to its
/// `AgentId`"*, presented.
///
/// marion is not this process's parent — the harness is — so both values ride the per-server `env`
/// block marion wrote into the node's declaration, beside each other, and the supervisor minted
/// both at the instant it claimed the node. That is what makes them checkable: `handler`'s
/// `resolve_caller` compares the token against its own table in constant time and derives every
/// gated fact from the registry, so this frame states *who* and nothing else.
///
/// **Absent is a refusal, and it always was, but the refusal has changed shape.** What used to be
/// missing here was `MARION_AGENT_TYPE` and `MARION_DEPTH`, and refusing was the only way to avoid
/// spawning ungated. Those are the supervisor's now. What must not be missing is the pair below —
/// an unattributed spawn cannot be gated at all, because there is no caller to read a depth or a
/// child count for, and the old literal fallback (`"unattributed-root"`) would now be a claim on
/// the wire rather than a string in a contract.
///
/// The token in particular is refused rather than sent empty: `SpawnCaller` has no `default` for
/// it, so an empty one would be a credential every process on the machine already has, presented as
/// if it were proof.
fn node_identity() -> Result<marion_proto::SpawnCaller, String> {
    identity_from(non_empty(AGENT_ID_ENV), non_empty(NODE_TOKEN_ENV))
}

/// The pure half, so the resolution is testable without an environment — the same split
/// [`report_refusal`] and [`caller_depth`] keep, and for the same reason: a refusal is a sentence
/// somebody has to read, and a sentence nothing pins is a sentence that drifts.
fn identity_from(
    agent_id: Option<String>,
    node_token: Option<String>,
) -> Result<marion_proto::SpawnCaller, String> {
    let agent_id = agent_id.ok_or_else(|| {
        format!(
            "marion: {AGENT_ID_ENV} is not set, so this bridge cannot say which node is asking for \
             a child. §6.1 step 2's gates are evaluated against the caller's own place in the tree, \
             and a spawn from nobody cannot be gated at all — the supervisor would have no depth \
             and no child count to read. Refusing rather than spawning unattributed. This is a \
             broken launch, not a rule: the key belongs in the node's marion server declaration."
        )
    })?;
    let node_token = node_token.ok_or_else(|| {
        format!(
            "marion: {NODE_TOKEN_ENV} is not set, so this bridge holds no proof that it serves \
             node {agent_id}. §5.4 binds a capability token to an `AgentId` and the supervisor \
             checks it, so an unproven claim would be refused on the socket anyway — and sending an \
             empty one would present a secret every process on this machine already has. A node \
             whose supervisor has restarted is in this case and it is not a mistake the caller \
             made: the token was minted in memory and died with that supervisor."
        )
    })?;
    Ok(marion_proto::SpawnCaller {
        agent_id: AgentId(agent_id),
        node_token,
    })
}

/// An environment value that is present **and says something**. An empty declaration key is a
/// declaration written wrong, never a value, for the reason every adapter's `env` block writes
/// these keys as present-or-absent and never empty.
fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// How much longer than the child's own wall clock marion will block a caller.
///
/// The child's `timeout_secs` bounds one thing: its harness invocation. Everything the supervisor
/// does around it — `git worktree add`, the config documents, the `--version` probe, the diff, the
/// contract write, `git worktree remove` — is outside that clock, so a bound of exactly
/// `timeout_secs` would expire on a healthy child that happened to be inside `git`.
///
/// **Deliberately generous, and deliberately finite.** Generous because expiring early hands the
/// caller a non-answer about a child that was about to finish. Finite because the bridge dispatches
/// frames on one thread: a read with no bound means no later frame from any caller is even read.
const WAIT_GRACE: Duration = Duration::from_secs(120);

/// The bound a `spawn` or a `wait` on this child may block for: **the node's own clock, plus
/// [`WAIT_GRACE`]**.
///
/// Derived from the request rather than invented beside it, and resolved through the same two
/// functions the supervisor resolves the node's real bound with — `handler`'s default for an absent
/// value and `run::effective_timeout`'s clamp — so marion cannot hold a caller for a period it
/// never agreed to run the node for, in either direction. Saturating, because the clamp caps the
/// request but the sum with the grace must still be a duration that exists.
fn wait_bound(timeout_secs: Option<u64>) -> Duration {
    run::effective_timeout(
        timeout_secs.unwrap_or(marion_supervisor::handler::DEFAULT_SPAWN_TIMEOUT_SECS),
    )
    .saturating_add(WAIT_GRACE)
}

/// A JSON array of strings, as the strings. Absent and empty are the same request — none — which is
/// what both `acceptance_criteria` and `writable_scope` read as one layer down.
fn string_list(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Tell marion the harness now has our tool list.
///
/// The harness connects `--mcp-config` servers **asynchronously and non-blockingly** (measured on
/// 2.1.220), so without this the root's first turn can go out before `mcp__marion__spawn` exists
/// and end in plain text with no error anywhere. See `root`'s module docs.
fn signal_ready() {
    if let Ok(path) = std::env::var(READY_FILE_ENV) {
        let _ = std::fs::write(path, b"ready\n");
    }
}

/// Serve MCP over stdio until the harness closes our stdin, **and then leave**.
///
/// # The hold that used to be here, and why its deletion is the point
///
/// This function ended `bg.join_all()`: at EOF it blocked until every backgrounded child had
/// finished. That was §5.7's exit rule read one level down — a supervisor MUST NOT exit while *"any
/// `spawn` is outstanding"* — and it was correct while a child's `Child`, its pipe and its thread
/// lived in **this** process, where leaving would have killed the child mid-run and left its
/// `SpawnIntent` journaled with no resolution.
///
/// §11 item 28 step 5 moved all of that to the supervisor. There is nothing outstanding here to
/// hold for: a child of this node is a node of the supervisor's, running on the supervisor's
/// thread, with the supervisor's `Child` and the supervisor's pid in its `Spawned` record. §5.7's
/// rule now binds the process it was always about, which is the one that owns the lifecycle — and
/// this process leaving is a courier hanging up.
///
/// **That is also what made the hold unfixable rather than merely unreached.** s16 measured a real
/// Claude Code harness ending its MCP server with SIGINT, SIGTERM 100 ms later and SIGKILL ~450 ms
/// after that, all pid-targeted, and **no EOF at all** — so the hold never ran in production, and a
/// bridge that ignored SIGTERM would have bought ~450 ms and died anyway. The child outliving the
/// bridge was §11 item 30's runaway; it is now the ordinary case, and
/// `tests/background_spawn.rs`'s `a_bridge_killed_mid_child_leaves_the_node_running_and_its_stream_growing`
/// is where it is measured rather than argued.
fn run_bridge() {
    let bg = background::Background::new();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(req) = bridge::parse(line) else {
            continue;
        };
        let mut answered_tools_list = false;
        let reply = match req {
            bridge::Request::Initialize { id } => Some(bridge::initialize_result(&id)),
            bridge::Request::ToolsList { id } => {
                answered_tools_list = true;
                Some(bridge::tools_list_result(&id))
            }
            bridge::Request::ToolsCall {
                id,
                name,
                arguments,
            } => Some(handle_tool_call(&bg, &id, &name, &arguments)),
            bridge::Request::Notification => None,
            bridge::Request::Unknown { id, method } => Some(bridge::method_not_found(&id, &method)),
        };
        if let Some(r) = reply {
            let _ = writeln!(stdout, "{r}");
            let _ = stdout.flush();
        }
        // After the flush, never before: the marker means "the harness has been sent the list".
        if answered_tools_list {
            signal_ready();
        }
    }
    // EOF, and nothing to wait for. The children of this node belong to the supervisor; this
    // process was the courier, and the courier is leaving.
}

#[cfg(test)]
mod main_tests {
    use super::*;

    /// **`wait` answers a handle it does not recognise with a sentence, not by blocking.**
    ///
    /// This test replaces `a_backgrounded_spawn_is_refused_by_name_rather_than_served_synchronously`,
    /// which pinned the `background` refusal that this change lifts. The refusal is gone because
    /// the feature arrived; what took its place is the obligation §5.4 attaches to a handle —
    /// *"the holder must be able to `wait` a node that may already have exited"* — and the way
    /// that obligation is most easily broken is a `wait` that waits on nothing.
    ///
    /// Three assertions, each failing against a plausible wrong implementation:
    ///
    /// 1. a `wait` with **no `task_id`** is refused rather than defaulting to "some child of
    ///    mine", which would block on work the caller was not asking about;
    /// 2. a `wait` on an **unknown** id is refused *and is honest about how narrow the lookup was*;
    /// 3. neither refusal needs an environment, which is how we know nothing was attempted.
    ///
    /// **Assertion 2 used to require the opposite of what it now requires, and that is the point.**
    /// It read: *says the lookup was the whole lookup (§5.4 scopes `wait` to descendants), so the
    /// caller cannot read it as "marion lost it"* — and the sentence it pinned ended *"marion did
    /// not search elsewhere and did not lose anything"*. The first half is true. The second is
    /// false in two ways that §5.4 itself makes reachable:
    ///
    /// * a **grandchild** is a descendant, so waiting on one is legitimate — and it is not in this
    ///   table, because it was started through its own parent's bridge instance. `Unknown` here is
    ///   marion's limit, not the caller's mistake.
    /// * the table lives only in this bridge **process**. A bridge that was restarted has lost the
    ///   exact handle it is being shown, and telling that caller "you are asking about nothing"
    ///   blames it for marion's own discontinuity.
    ///
    /// So the assertion now pins the honest sentence and pins the retracted claim as *absent*. A
    /// test that asserted the old wording would have kept the falsehood alive, which is why it was
    /// changed rather than accommodated.
    ///
    /// It needs no `MARION_REPO` and starts no child, so it is a unit test rather than the
    /// end-to-end control in `tests/background_spawn.rs`.
    #[test]
    fn wait_refuses_by_name_rather_than_blocking_on_a_child_it_never_started() {
        let bg = marion_supervisor::background::Background::new();

        let no_id = handle_tool_call(&bg, &serde_json::json!(1), "wait", &serde_json::json!({}));
        assert_eq!(no_id["result"]["isError"], serde_json::json!(true));
        let text = no_id["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            text.contains("task_id"),
            "a wait with nothing to wait on must name what is missing, got: {text}"
        );

        let unknown = handle_tool_call(
            &bg,
            &serde_json::json!(2),
            "wait",
            &serde_json::json!({"task_id": "task-never-started"}),
        );
        assert_eq!(unknown["result"]["isError"], serde_json::json!(true));
        let text = unknown["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            text.contains("task-never-started"),
            "the refusal names the id it was asked about, got: {text}"
        );
        assert!(
            text.contains("grandchild"),
            "and names the §5.4-permitted relationship this lookup cannot resolve, so `Unknown` is \
             not read as `you may not wait on that`, got: {text}"
        );
        assert!(
            text.contains("restarted"),
            "and admits the case where the handle was marion's to keep and marion lost it, got: \
             {text}"
        );
        assert!(
            !text.contains("did not lose"),
            "and never claims nothing was lost, which is the sentence this refusal used to end \
             with and cannot support, got: {text}"
        );
        assert!(
            !text.contains("MARION_REPO"),
            "and it precedes every environment check, so nothing was attempted: {text}"
        );
    }

    /// **The whole refusing half of the table, matched on the typed variant rather than on prose.**
    ///
    /// One case per parameter that made marion perform a different verb quietly. `isolation` gets
    /// both of its unimplemented values, because they fail in opposite directions and a refusal
    /// that caught only one would leave the worse one standing: `shared-cwd` silently *added*
    /// containment the caller did not ask for, while `remote` silently ran on the operator's own
    /// machine. Each case also asserts the message **names the parameter**, since a caller that
    /// cannot tell which of its eleven keys was rejected has been told almost nothing.
    #[test]
    fn every_unimplemented_spawn_parameter_is_refused_by_name() {
        use spawn::SpawnError::*;
        for (label, args, needle) in [
            (
                "allow_concurrent_writes",
                serde_json::json!({"allow_concurrent_writes": true}),
                "allow_concurrent_writes",
            ),
            (
                "isolation: shared-cwd",
                serde_json::json!({"isolation": "shared-cwd"}),
                "shared-cwd",
            ),
            (
                "isolation: remote",
                serde_json::json!({"isolation": "remote"}),
                "remote",
            ),
            (
                "verification",
                serde_json::json!({"verification": ["cargo test -p foo"]}),
                "verification",
            ),
        ] {
            let e = unimplemented_parameter(&args).unwrap_or_else(|| {
                panic!(
                    "{label} is declared and unimplemented, so it must be refused rather than \
                     dropped"
                )
            });
            let msg = e.to_string();
            assert!(
                msg.contains(needle),
                "{label}: the refusal must name what was rejected, got: {msg}"
            );
            assert!(
                msg.contains("not implemented"),
                "{label}: and say it is unimplemented, so the caller can tell \"marion will not\" \
                 from \"marion could not\", got: {msg}"
            );
        }
        // The variants themselves, so a refusal cannot be renamed into a different meaning without
        // this failing: `isolation` carries the value it rejected, which is what lets a caller fix
        // the call rather than guess.
        assert!(matches!(
            unimplemented_parameter(&serde_json::json!({"allow_concurrent_writes": true})),
            Some(ConcurrentWritesUnimplemented)
        ));
        assert!(matches!(
            unimplemented_parameter(&serde_json::json!({"verification": ["x"]})),
            Some(VerificationUnimplemented)
        ));
        match unimplemented_parameter(&serde_json::json!({"isolation": "remote"})) {
            Some(IsolationUnimplemented(v)) => assert_eq!(v, "remote"),
            other => panic!("expected the value to be carried, got {other:?}"),
        }
    }

    /// **The accepting half, which is the half that a careless refusal breaks.**
    ///
    /// Every one of these describes something marion actually does, so refusing any of them would
    /// cost a caller a working spawn and buy no honesty:
    ///
    /// * absent keys ask for nothing;
    /// * `background: false` and `isolation: "worktree"` name marion's own behaviour;
    /// * an **empty** `verification` requests no commands, so no evidence is missing;
    /// * `name` and `allow_concurrent_writes` are the "merely absent" half of §11 item 23 —
    ///   dropped, but contradicting no answer the caller receives, and deliberately still accepted.
    ///
    /// This is the assertion the refusal above cannot make: it only ever looks at the rejecting
    /// values, so an over-broad check would pass it and fail here.
    #[test]
    fn every_value_marion_actually_performs_is_still_accepted() {
        for (label, args) in [
            ("nothing optional at all", serde_json::json!({})),
            (
                "background: false",
                serde_json::json!({"background": false}),
            ),
            (
                "isolation: worktree",
                serde_json::json!({"isolation": "worktree"}),
            ),
            (
                "an empty verification list",
                serde_json::json!({"verification": []}),
            ),
            (
                // `background: true` is here rather than in the refusing table because it is now
                // implemented. Its presence in *this* list is the assertion that the refusal was
                // lifted rather than merely reworded.
                "background: true, now that it is implemented",
                serde_json::json!({"background": true}),
            ),
            (
                // `false` is §6.6's default and marion honours it structurally: every child gets
                // its own worktree, so there is never a second writer in an occupied cwd. It is
                // here, not in the refusing table, and the difference between the two values is
                // the whole of §11 item 23's correction.
                "allow_concurrent_writes: false",
                serde_json::json!({"allow_concurrent_writes": false}),
            ),
            (
                "the dropped-but-harmless key",
                serde_json::json!({"name": "impl-auth"}),
            ),
        ] {
            assert!(
                unimplemented_parameter(&args).is_none(),
                "{label}: marion performs this, so refusing it would break a working spawn — got \
                 {:?}",
                unimplemented_parameter(&args)
            );
        }
    }

    /// **§5.4's `report` row as a table, so every row is stated rather than implied.**
    ///
    /// The end-to-end witness is `tests/report_on_a_root.rs`, which drives this binary the way a
    /// `LaunchOnly` node's MCP client does. This is the decision underneath it. The rows where the
    /// depth cannot be read at all are the test below's: they are a different refusal, because they
    /// are a different fact about the world.
    #[test]
    fn only_a_node_that_is_known_to_be_the_root_has_its_report_refused() {
        assert_eq!(
            report_refusal(Some("0".into())).as_deref(),
            Some(bridge::REPORT_ON_A_ROOT),
            "§5.4 rejects `report` on a root, and depth 0 is what being the root means (§3.1)"
        );
        for (label, depth) in [
            ("a child", Some("1".to_string())),
            ("a grandchild", Some("2".to_string())),
            // Whitespace survives an env round trip on some shells; the depth is the number.
            ("a padded depth", Some(" 1 ".to_string())),
        ] {
            assert_eq!(
                report_refusal(depth.clone()),
                None,
                "{label}: `report` is a node-with-a-contract's return path, and refusing it here \
                 would break the verb for every child in the tree"
            );
        }
        // The refusal a caller receives has to be actionable, not a code.
        for needle in ["§5.4", "contract", "root", "spawn"] {
            assert!(
                bridge::REPORT_ON_A_ROOT.contains(needle),
                "the refusal must name the rule and the way out ({needle:?} missing)"
            );
        }
    }

    /// **A depth marion cannot read is a refusal, and not the same refusal as a root's.**
    ///
    /// This was the hole [`report_refusal`] was written to close and then left open one row wider
    /// than it looked: an absent, empty or unparsable `MARION_DEPTH` fell out of the parse as
    /// `None` and the bridge answered `report recorded`, `isError: false` — the exact receipt for a
    /// payload nothing stages that the refusal exists to delete, reachable by any bridge whose
    /// declaration was written wrong. It is the repository's recurring defect: a check that reports
    /// success by failing to look. An absence must be recorded as an absence.
    ///
    /// **Two sentences, because they are two different pieces of news.** `REPORT_ON_A_ROOT` is a
    /// claim about the node — *you are the root, roots have no contract* — and marion has no
    /// grounds for it here: it does not know what this node is. So this one says what it actually
    /// observed, names the key, and points at the launch rather than at the caller's behaviour. A
    /// node that reads them has to be able to tell a rule it broke from a bridge that was started
    /// wrong, because only one of those is something it can act on.
    #[test]
    fn a_report_whose_callers_depth_cannot_be_read_is_refused_rather_than_recorded() {
        for (label, depth) in [
            ("a bridge that was told nothing", None),
            ("an empty value", Some(String::new())),
            ("whitespace only", Some("   ".to_string())),
            ("a depth that is not a number", Some("deep".to_string())),
            ("a negative depth", Some("-1".to_string())),
            // Wider than u32: the parse fails, and failing must mean refusing here too.
            (
                "a depth too large to be one",
                Some("99999999999999".to_string()),
            ),
        ] {
            let msg = report_refusal(depth.clone()).unwrap_or_else(|| {
                panic!(
                    "{label}: marion cannot establish the caller's depth, so it cannot authorize \
                     the call — answering \"report recorded\" is a receipt for a payload nothing \
                     stages"
                )
            });
            assert!(
                msg.contains(DEPTH_ENV),
                "{label}: the refusal must name the key that is missing, since the fix is in the \
                 node's declaration and not in the call: {msg}"
            );
            assert!(
                !msg.contains(bridge::REPORT_ON_A_ROOT),
                "{label}: marion does not know this node is a root, so it must not claim to — a \
                 broken launch and a rule violation must not read alike: {msg}"
            );
        }
    }

    /// **A bridge that cannot prove which node it serves refuses, rather than spawning
    /// unattributed** — §5.4's token, at the point where it is presented.
    ///
    /// This replaces `a_bridge_that_was_not_told_which_node_it_serves_refuses_rather_than_spawning_ungated`,
    /// and the replacement is the whole of §11 item 28 step 5 in one test. That test pinned refusals
    /// on `MARION_AGENT_TYPE` and `MARION_DEPTH`, because this process evaluated §6.1 step 2's gates
    /// and could not evaluate them without both. It does not evaluate them any more: the supervisor
    /// derives the caller's type, depth and live-child count from the registry it wrote itself, so
    /// a caller that *stated* any of them could lie about them and now cannot state them at all.
    ///
    /// What must still be present is the pair below, and each is refused for its own reason:
    ///
    /// * **the id**, because a spawn from nobody cannot be gated at all — there is no caller whose
    ///   depth or child count the supervisor could read. The old literal fallback
    ///   (`"unattributed-root"`) was a string in a contract; over the socket it would be a claim.
    /// * **the token**, because §5.4 binds a capability to an `AgentId` and an empty one is a
    ///   secret every process on this machine already has, presented as proof. `SpawnCaller` has no
    ///   `default` for it precisely so that it cannot be sent absent.
    #[test]
    fn a_bridge_that_cannot_prove_which_node_it_serves_refuses_rather_than_spawning_unattributed() {
        for (label, agent_id, token, expected) in [
            ("a bridge that was told nothing", None, None, AGENT_ID_ENV),
            (
                "an id with no proof behind it",
                Some("019f-node".to_string()),
                None,
                NODE_TOKEN_ENV,
            ),
            (
                "a token attached to nobody",
                None,
                Some("tok".to_string()),
                AGENT_ID_ENV,
            ),
        ] {
            let e = identity_from(agent_id, token).expect_err("an unprovable caller must refuse");
            assert!(
                e.contains(expected),
                "{label}: the refusal must name the key that is missing, since the fix is in the \
                 node's declaration and not in the call: {e}"
            );
            assert!(
                e.contains("Refusing") || e.contains("refused"),
                "{label}: and say that nothing was attempted: {e}"
            );
        }
        let c = identity_from(Some("019f-node".into()), Some("tok-abc".into()))
            .expect("a declaration carrying both resolves");
        assert_eq!(c.agent_id.0, "019f-node");
        assert_eq!(c.node_token, "tok-abc");
    }

    /// **The bound marion will hold a caller for is the node's own clock plus the grace** — and a
    /// wall clock no `Instant` can represent does not detonate on the way to it.
    ///
    /// Both halves have a history. The clamp is B1: `timeout_secs` is caller-controlled and typed
    /// `u64`, and `Instant + Duration` panics on overflow — which used to happen on this process's
    /// own dispatch thread. And the *default* is read from `handler`'s constant rather than written
    /// again here, because the bridge no longer resolves the number it sends: an absent
    /// `timeout_secs` goes over the wire absent, the supervisor resolves it, and a `900` spelled
    /// twice is how the tool's promise and the node's real clock come to disagree.
    #[test]
    fn the_bound_a_wait_may_block_is_the_nodes_own_clock_plus_the_grace() {
        assert_eq!(
            wait_bound(Some(60)),
            Duration::from_secs(60) + WAIT_GRACE,
            "a stated clock is the caller's, and the grace is for the work around the run"
        );
        assert_eq!(
            wait_bound(None),
            Duration::from_secs(marion_supervisor::handler::DEFAULT_SPAWN_TIMEOUT_SECS)
                + WAIT_GRACE,
            "an absent clock resolves to the same default the supervisor will resolve it to"
        );
        assert_eq!(
            wait_bound(Some(u64::MAX)),
            Duration::from_secs(run::MAX_TIMEOUT_SECS) + WAIT_GRACE,
            "a number no clock can hold is clamped to the bound marion actually enforces, not \
             passed on to be added to an `Instant`"
        );
    }

    /// **The peer invariant, pinned: an MCP entry point owns nothing and has no launch path of its
    /// own.**
    ///
    /// §11 item 28 step 5's whole content is that the bridge stopped running children — the
    /// supervisor owns every process and every MCP surface is a socket client. Step 6 made
    /// `marion run` the same shape. So the two top-level surfaces marion exposes, the TUI client
    /// and the MCP server, are **peers**: both are socket clients, neither is privileged, and
    /// neither may start a node itself.
    ///
    /// **This is a rule, not a habit, and this test is where it is a rule.** The defect it forbids
    /// is not hypothetical — it is the exact arrangement item 28 was written to remove, and it is
    /// easy to re-introduce by accident, because `run::run_spawn` and `root::launch` are `pub`, are
    /// in this crate, and do precisely what a careless "just start it here" edit would reach for.
    /// A bridge that called one would work in a test, produce a real child, and journal it under a
    /// supervisor that had never heard of it.
    ///
    /// It reads this file's own source because that is the only way to assert an **absence** of a
    /// call. A type-level version would need a capability parameter threaded through every arm to
    /// exclude functions that are merely `pub` in the same crate, which is a large change to
    /// forbid a small thing.
    ///
    /// **Two things are cut before the scan, and both are the difference between naming the launch
    /// path and calling it.** Comment lines go, because the module docs discuss `root::prepare` at
    /// length and should keep being able to. Everything from `#[cfg(test)]` goes, because this test
    /// must name the very symbols it forbids — a scan that read its own source could never pass,
    /// and one that worked around that by spelling the symbols obliquely would stop failing when
    /// the real thing was added.
    #[test]
    fn the_mcp_entry_point_has_no_spawn_path_of_its_own() {
        let src = include_str!("main.rs");
        let production = src
            .split_once("#[cfg(test)]")
            .expect("this file has a test module")
            .0;
        let code: String = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // Every `pub` way to start a node from inside this crate, plus the standard-library call
        // any hand-rolled path would have to bottom out in.
        for forbidden in [
            "run_spawn",
            "root::launch",
            "root::prepare",
            "prepare_watched",
            "launch_owned",
            "Command::new",
        ] {
            assert!(
                !code.contains(forbidden),
                "`{forbidden}` is reachable from the MCP entry point. The supervisor owns every \
                 process (§11 item 28 step 5): this binary's `mcp` surface is a socket client and \
                 must ask for a child over `agent/spawn` rather than start one. If this is a \
                 deliberate change, it re-introduces the defect item 28 removed."
            );
        }
        // The positive half, so the test cannot pass by this file having become empty or by the
        // dispatch having been moved somewhere the scan does not read.
        assert!(
            code.contains("courier::spawn(") && code.contains("AgentSpawnParams"),
            "the spawn arm must still reach the supervisor over the socket — this half is what \
             stops the scan above from passing because the dispatch was deleted or moved"
        );
    }

    /// **Every tool marion declares is dispatched — none is declared and then answered as if it
    /// did not exist.**
    ///
    /// This repo's standing rule is that a surface marion advertises and does not implement must
    /// **refuse by name** (`isolation`, `verification`, `allow_concurrent_writes`), never fall
    /// through to something that reads like a different failure. `handle_tool_call`'s last arm
    /// answers `marion: no tool {name}` — the correct answer for a name marion never declared, and
    /// the wrong one for a name in its own `tools/list`, because a caller reading it would
    /// conclude the tool does not exist when marion had just said it does.
    ///
    /// The arms are exercised with no environment, so most return a launch refusal — that is the
    /// point: what is asserted is that the name **was routed**, not that the call succeeded. A tool
    /// added to `bridge::tools` without an arm in `handle_tool_call` fails here.
    #[test]
    fn every_declared_tool_is_dispatched_rather_than_answered_as_unknown() {
        let bg = background::Background::new();
        let declared = bridge::tools();
        let names: Vec<&str> = declared
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.is_empty(), "marion declares at least one tool");
        for name in &names {
            let answer = handle_tool_call(&bg, &serde_json::json!(1), name, &serde_json::json!({}));
            let text = answer["result"]["content"][0]["text"].as_str().unwrap();
            assert!(
                !text.contains("no tool"),
                "`{name}` is declared in `tools/list` and answered as though it were not: {text:?}"
            );
        }
        // The converse, so the assertion above cannot be satisfied by deleting the fallthrough:
        // a name marion never declared is still refused by name.
        let unknown = handle_tool_call(
            &bg,
            &serde_json::json!(1),
            "teleport",
            &serde_json::json!({}),
        );
        assert!(
            unknown["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no tool teleport"),
            "an undeclared tool is refused by name"
        );
        assert_eq!(unknown["result"]["isError"], true);
    }

    /// **`list` answers about the caller's own subtree and nothing else.**
    ///
    /// The filter is the whole authorization boundary for this verb: `tree/subscribe` answers with
    /// every node in the project — other roots, unrelated subtrees, the whole fleet — and §5.4
    /// permits `list` only *"descendants or parent, plus `allow_peers` siblings"*. An unfiltered
    /// `list` would therefore not be a wider answer to the same question; it would be a different
    /// question, answered without authorization.
    ///
    /// Four properties, each failing against a plausible wrong filter: a grandchild **is** included
    /// (descendants, not children — a `children_of` that stopped at one level would pass a weaker
    /// test); an unrelated root and its child are **not**; a sibling is **not**, because
    /// `allow_peers` is not built and a filter that admitted one would be granting what nothing
    /// checks; and the caller itself is **not**, so a model cannot come to address its own handle.
    #[test]
    fn list_answers_the_callers_own_subtree_and_not_the_fleet() {
        use marion_core::harness::Harness;
        use marion_core::node::{NodeState, ReapState};
        use marion_proto::model::NodeSummary;

        fn node(id: &str, parent: Option<&str>) -> NodeSummary {
            NodeSummary {
                agent_id: AgentId(id.into()),
                parent_id: parent.map(|p| AgentId(p.into())),
                name: None,
                agent_type: "codex-impl".into(),
                harness: Harness::Codex,
                depth: 1,
                state: NodeState::Running,
                reap_state: ReapState::Live,
                timeout: marion_core::encoding::Duration::from_secs(60),
            }
        }
        let fleet = vec![
            node("me", Some("my-parent")),
            node("my-parent", None),
            node("my-sibling", Some("my-parent")),
            node("my-child", Some("me")),
            node("my-grandchild", Some("my-child")),
            node("other-root", None),
            node("other-child", Some("other-root")),
        ];
        let mut got: Vec<String> = descendants_of(&AgentId("me".into()), &fleet)
            .iter()
            .map(|n| n.agent_id.0.clone())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec!["my-child".to_string(), "my-grandchild".to_string()],
            "§5.4 scopes `list` to the caller's descendants: a grandchild is one, and a sibling, a \
             parent, an unrelated root and the caller itself are not"
        );
        // A caller that delegated nothing gets an empty set rather than the fleet — the case an
        // absent or inverted filter would most obviously get wrong.
        assert!(
            descendants_of(&AgentId("other-child".into()), &fleet).is_empty(),
            "a node with no children sees no nodes"
        );
    }

    /// **A subtree walk terminates even on a journal that names a cycle.**
    ///
    /// A cycle cannot arise from a journal marion wrote — a node's `parent_id` is fixed at its
    /// `SpawnIntent` and never edited — so this asserts a property of the *loop* rather than a
    /// belief about the input. It is worth pinning because the failure mode is not a wrong answer
    /// but a hang: `list` is called on a model's turn, and a bridge spinning here stops reading
    /// every later frame from every caller, which is the same single-threaded-dispatch argument
    /// `courier::Conn::bound` is written for.
    #[test]
    fn a_subtree_walk_terminates_on_a_cycle_rather_than_spinning() {
        use marion_core::harness::Harness;
        use marion_core::node::{NodeState, ReapState};
        use marion_proto::model::NodeSummary;

        let cyclic = |id: &str, parent: &str| NodeSummary {
            agent_id: AgentId(id.into()),
            parent_id: Some(AgentId(parent.into())),
            name: None,
            agent_type: "codex-impl".into(),
            harness: Harness::Codex,
            depth: 1,
            state: NodeState::Running,
            reap_state: ReapState::Live,
            timeout: marion_core::encoding::Duration::from_secs(60),
        };
        // a -> b -> a, with the caller pointing into it.
        let nodes = vec![cyclic("a", "b"), cyclic("b", "a"), cyclic("a", "me")];
        let found = descendants_of(&AgentId("me".into()), &nodes);
        assert!(
            found.len() <= nodes.len(),
            "no node is reported twice, so the walk visited each at most once"
        );
    }

    #[test]
    fn documented_state_precedence_is_resolved_beneath_the_project_hash() {
        let root = std::path::Path::new("/canonical/project");
        let state = state_dir(Some("/explicit"), Some("/xdg"), Some("/home")).unwrap();
        let project = ProjectDir::new(&state, root);
        assert_eq!(
            project.path().parent(),
            Some(std::path::Path::new("/explicit"))
        );
        assert_eq!(
            project.path().file_name().unwrap().to_string_lossy().len(),
            12
        );
    }
}
