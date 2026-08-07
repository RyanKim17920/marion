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

use std::io::{BufRead, Read, Write};

use marion_core::ids::{RAND_BYTES, new_task_id as core_new_task_id};
use marion_core::paths::{ProjectDir, state_dir};
use marion_supervisor::root::{AGENT_ID_ENV, AGENT_TYPE_ENV, DEPTH_ENV, READY_FILE_ENV};
use marion_supervisor::{bridge, detach, run, spawn};

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

/// `spawn` runs the child and returns its completed contract — or, with `background: true`,
/// returns a handle immediately and runs the child on a thread this bridge owns; `wait` collects
/// that child; `report` stages the child's payload, which the parent's own `spawn` then returns.
///
/// **`bg` is threaded through rather than being a `static`** so the table's lifetime is the
/// bridge's, and so the unit tests below can each hold their own. A process-wide table would make
/// one test's live-children count depend on which other tests had run — the exact defect class this
/// repo keeps finding, one level down.
fn handle_tool_call(
    bg: &marion_supervisor::background::Background,
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
            // Two values and not one: the tree is per-spawn (`run::SpawnRequest::repo`) and the
            // environment is per-supervisor, which is the split `run::Env` was carrying wrongly.
            // In *this* process they come from the same declaration, which is exactly why the type
            // system has to keep them apart — a bridge serves one node, so the two cannot be told
            // apart by observation here.
            let Ok((env, repo)) = spawn_env() else {
                return bridge::tool_result(id, "marion: MARION_REPO is not set", true);
            };
            // §6.1 step 2's gates read the caller's agent type and depth, and this bridge is the
            // only place that knows which node it is serving. A bridge that was not told cannot
            // evaluate them, so it refuses rather than spawning ungated — which is exactly the
            // hazard this path exists to close, and the same shape as `spawn_env`'s refusal above.
            //
            // The **live-children count** is stamped on here rather than inside `caller()`, because
            // it is the one field of the three that is a property of this bridge's own table and
            // not of the declaration marion wrote. It was the constant
            // `LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER = 0` until backgrounding landed; `run_spawn`
            // reads it off the `Caller` and refuses before every side effect.
            let caller = match caller(&requester()) {
                Ok(c) => run::Caller {
                    live_children: bg.live_children(),
                    ..c
                },
                Err(e) => return bridge::tool_result(id, &e, true),
            };
            let req = run::SpawnRequest {
                repo,
                agent_type: args["agent_type"]
                    .as_str()
                    .unwrap_or("codex-impl")
                    .to_string(),
                prompt: args["prompt"].as_str().unwrap_or_default().to_string(),
                acceptance_criteria: args["acceptance_criteria"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                writable_scope: args["writable_scope"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                timeout_secs: args["timeout_secs"].as_u64().unwrap_or(900),
                // Absent is not empty: `None` falls back to the agent type's own `model` key
                // (§3.1), which is what makes a `gemini` or `opencode` spawn launchable without
                // the parent having to know which harness needs a model and in what spelling.
                model: args["model"].as_str().map(str::to_string),
            };
            let Ok(task_id) = new_task_id() else {
                return bridge::tool_result(id, "marion: could not generate task id", true);
            };
            // **§5.4's `background`, read for its value.** Absent and `false` both mean "block",
            // which is what marion did for every spawn until now and what the schema's own comment
            // pinned for M1. `true` starts the child on a thread and returns a handle in the same
            // frame — see `background`'s module docs for why the thread lives in this process and
            // what that costs.
            //
            // **§6.1 step 2 is evaluated here as well as inside `run_spawn`, and that is not
            // drift.** It is the *same function* — `marion_core::agent_type::check_spawn_gates`,
            // pure and idempotent — called from two places, not two implementations of one rule.
            // §9's warning is about two derivations that can disagree; this cannot.
            //
            // It has to be here because of what a handle *means*. `run_spawn` refuses before every
            // side effect, which is right, but on the background path that refusal happens inside
            // the thread and would reach the caller only through a later `wait` — so a `spawn`
            // that marion had already decided to refuse would answer *"the child is running"*.
            // That is a lie in the result slot, and deleting lies in the result slot is what this
            // whole change is about. A refused background spawn is refused in the same frame that
            // asked for it, in the same shape a synchronous one takes.
            //
            // `run_spawn` keeps its own call: it is a public library entry point with callers
            // (`marion run`, the tests) that do not come through here, and a gate that only ran at
            // one call site is the hole `depth_gate` was written about.
            if args["background"].as_bool() == Some(true) {
                if let Err(e) = marion_core::agent_type::check_spawn_gates(
                    &caller.agent_type,
                    caller.depth,
                    caller.live_children,
                ) {
                    return bridge::spawn_result(id, &req.agent_type, Err(e.into()));
                }
                let started = bg.start(env, req, task_id, caller);
                return bridge::background_result(id, &started);
            }
            // A child that ran and failed and a spawn that never launched are the same news to
            // the parent, and used to arrive in two different shapes. `bridge::spawn_result` is
            // where that is decided, in one place, so both read alike.
            bridge::spawn_result(
                id,
                &req.agent_type,
                run::run_spawn(&env, &req, &task_id, &caller),
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
            match bg.wait(task_id) {
                marion_supervisor::background::Wait::Finished(outcome) => {
                    // The agent type for the "could not be launched" line comes from the table,
                    // not from these arguments: a `wait` carries no `agent_type`, and inventing
                    // one would put a name in a refusal that names the wrong thing.
                    bridge::spawn_result(id, "backgrounded", *outcome)
                }
                marion_supervisor::background::Wait::Unknown => bridge::wait_unknown(id, task_id),
                marion_supervisor::background::Wait::AlreadyCollected(what) => {
                    bridge::wait_already_collected(id, task_id, what)
                }
                // Deliberately **not** routed through `spawn_result`: every other arm here carries
                // an outcome, and this one carries the absence of an outcome about a child that is
                // still going. Flattening it into the contract-shaped reply would make "still
                // running" indistinguishable from "ran and produced nothing", which is the exact
                // confusion §7.6's worked example is about.
                marion_supervisor::background::Wait::StillRunning { agent_type, waited } => {
                    bridge::wait_still_running(id, task_id, &agent_type, waited.as_secs())
                }
            }
        }
        other => bridge::tool_result(id, &format!("marion: no tool {other}"), true),
    }
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
/// [`caller_depth`] is shared with [`caller_from`] so the two gates cannot drift into reading the
/// same key by different rules; the consequence clause differs because the verbs differ. All four
/// adapters emit `MARION_DEPTH`, swept by `marion_harness::adapter`'s own test, so the only caller
/// that can land in the second refusal is a hand-started bridge.
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
/// The two refusals differ in what they protect — one an ungated `spawn`, one an unattributable
/// `report` — but not in what went wrong, and two hand-rolled parses of one environment variable
/// with two failure directions is how [`report_refusal`] came to serve where [`caller_from`]
/// refused. Sharing the parse makes the direction a single decision: unreadable is `Err`, and every
/// caller of this decides only what to say about it.
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

/// The node this bridge instance is serving, which becomes `TaskContract.requester`.
///
/// §9: *"`requester` for a top-level `spawn` is the root's `AgentId`."* marion is not this
/// process's parent — the harness is — so the id rides the server declaration marion wrote
/// (`root::mcp_config_json`) rather than an inherited fd. The literal fallback is what a
/// hand-started bridge gets: honest about being unattributed rather than inventing a uuid that
/// names no agent-dir.
fn requester() -> String {
    std::env::var(AGENT_ID_ENV).unwrap_or_else(|_| "unattributed-root".into())
}

/// The caller of this `spawn`, rebuilt from the declaration marion wrote (§6.1 step 2).
///
/// The whole of the gate's input arrives through the per-server MCP `env` block, for the reason
/// [`requester`] gives about the id: marion is not this process's parent. `agent_type` names the
/// type whose `max_depth` / `max_concurrent_children` the gate reads, and `depth` says where in the
/// tree this node sits, with the root at 0.
///
/// **Absent or unresolvable is a refusal, not a default**, and the two candidate defaults are both
/// wrong in the same direction: assuming depth 0 makes every node look like a root, and assuming
/// `DEFAULT_MAX_DEPTH` makes the per-type key a constant nothing reads. Either would restore
/// exactly the unbounded recursion this is here to stop, silently. All four adapters emit both
/// keys — `marion_harness::adapter`'s own test sweeps `Harness::ALL` — so the only caller that can
/// land here is a hand-started bridge, which is the one that most needs to be told.
///
/// [`report_refusal`] now fails in the same direction on the same key, through [`caller_depth`].
fn caller(agent_id: &str) -> Result<run::Caller, String> {
    caller_from(
        agent_id,
        std::env::var(AGENT_TYPE_ENV).ok(),
        std::env::var(DEPTH_ENV).ok(),
    )
}

/// The pure half, so the resolution is testable without an environment.
fn caller_from(
    agent_id: &str,
    agent_type: Option<String>,
    depth: Option<String>,
) -> Result<run::Caller, String> {
    let name = agent_type.ok_or_else(|| {
        format!(
            "marion: {AGENT_TYPE_ENV} is not set, so this bridge does not know which agent type is \
             calling and cannot read its max_depth (§6.1 step 2). Refusing rather than spawning \
             ungated."
        )
    })?;
    let agent_type = marion_core::agent_type::builtin(&name).ok_or_else(|| {
        format!(
            "marion: {AGENT_TYPE_ENV}={name:?} names no known agent type, so its spawn gates \
                 cannot be read. Refusing rather than spawning ungated."
        )
    })?;
    // The same read as [`report_refusal`]'s, deliberately — see [`caller_depth`]. Only the
    // consequence clause is this gate's own, because only this gate creates anything.
    let depth = caller_depth(depth).map_err(|e| {
        format!(
            "marion: {e}, so this bridge does not know how deep in the tree it is and cannot \
             enforce max_depth (§6.1 step 2). Refusing rather than spawning ungated."
        )
    })?;
    Ok(run::Caller {
        agent_id: agent_id.to_string(),
        agent_type,
        depth,
        // A placeholder the *caller* of this function overwrites from the bridge's own background
        // table before any gate reads it (`handle_tool_call`). It is not a default that stands: a
        // zero left standing here would be `LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER` reintroduced
        // under a new name, with the concurrency gate inert again and nothing saying so.
        live_children: 0,
    })
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

/// The bridge's half of a spawn: the supervisor-wide environment, **and** the one tree this bridge
/// serves, returned as two values because they are two facts. See [`run::SpawnRequest::repo`].
fn spawn_env() -> Result<(run::Env, std::path::PathBuf), ()> {
    let repo = std::path::PathBuf::from(std::env::var("MARION_REPO").map_err(|_| ())?);
    let repo = repo.canonicalize().map_err(|_| ())?;
    let legacy = std::env::var("MARION_STATE").ok();
    let documented = std::env::var("MARION_STATE_DIR").ok();
    let explicit = documented.as_deref().or(legacy.as_deref());
    let state = state_dir(
        explicit,
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .ok_or(())?;
    let (auth, base_url) = auth_from_env(
        std::env::var(marion_supervisor::root::AUTH_ENV).ok(),
        std::env::var(marion_supervisor::root::BASE_URL_ENV).ok(),
    );
    Ok((
        run::Env {
            // §2's key — the git common dir, not the cwd. The bridge is spawned *inside* the node's
            // worktree in some configurations, so this is the call that stops a child from
            // journalling into a project of its own. `marion run` and `root::prepare` make the same
            // one. It is also precisely why the repository cannot live here: this hash is the same
            // for `/r` and for every linked worktree of `/r`, and a worktree is made from one
            // tree's HEAD.
            project_dir: ProjectDir::new(&state, &marion_supervisor::socket::project_root(&repo)),
            bridge: std::env::current_exe().unwrap_or_else(|_| "marion-supervisor".into()),
            base_url,
            auth,
        },
        repo,
    ))
}

/// **How `--live` crosses a spawn hop.**
///
/// The bridge serves a node marion did not start, so its only channel is the per-server `env` block
/// marion wrote into that node's declaration. `MARION_AUTH` is the key that carries the mode; the
/// pure half is here so the hop is testable without an environment.
///
/// Three rules, and each is the answer to a way this could go quietly wrong:
///
/// 1. **`inherited` means the child is live too, and marion names no endpoint for it.** A live root
///    delegating to a canned child would launch that child against a server that is not running —
///    the failure that motivated part 2 — and a canned root delegating to a live child would spend
///    the operator's money without anyone asking for it.
/// 2. **An absent `MARION_AUTH` is `Canned`**, which is what every declaration written before the
///    key existed meant. It is *not* inferred from an absent `MARION_BASE_URL`: guessing live from a
///    missing key points a real credential somewhere marion did not choose.
/// 3. **An unrecognised value is `Canned` too, not a guess in the expensive direction.** The two
///    errors are not symmetric — see [`marion_harness::Auth::from_wire`].
///
/// The empty string is the state this function exists to make impossible downstream: a
/// `MARION_BASE_URL=""` (which is what `unwrap_or_default()` used to write into a live root's
/// declaration) is treated as absent, never as an endpoint.
fn auth_from_env(
    auth: Option<String>,
    base_url: Option<String>,
) -> (marion_harness::Auth, Option<String>) {
    let auth = auth
        .as_deref()
        .and_then(marion_harness::Auth::from_wire)
        .unwrap_or(marion_harness::Auth::Canned);
    let base_url = match auth {
        // Live means marion overlays no endpoint on the child, exactly as `marion run --live`
        // overlays none on the root. Any inherited value is ignored rather than obeyed.
        marion_harness::Auth::Inherited => None,
        marion_harness::Auth::Canned => Some(
            base_url
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| "http://127.0.0.1:8099/v1".into()),
        ),
    };
    (auth, base_url)
}

fn new_task_id() -> std::io::Result<marion_core::contract::TaskId> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut entropy = [0; RAND_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
    Ok(core_new_task_id(ms, entropy))
}

/// Serve MCP over stdio until the harness closes our stdin — **and then wait for any child still
/// running before leaving.**
///
/// The hold is §5.7's exit rule one level down: a supervisor MUST NOT exit while *"any `spawn` is
/// outstanding"*, and while `spawn` was synchronous that was free — a spawn in flight meant this
/// loop was inside `handle_tool_call` and could not reach EOF. Backgrounding makes it a real
/// obligation. Exiting at EOF with a live child would kill that child's process group mid-run and
/// leave a `SpawnIntent` journaled with no resolution, which §7.2 reads as a node marion **lost** —
/// asserting an accident about a process marion in fact chose to abandon.
///
/// **This hold is unreachable under a real Claude Code harness, and that is measured.** s16
/// (2026-08-06, `claude` 2.1.222, four runs) found **no EOF at all**: SIGINT, SIGTERM 100 ms later,
/// then SIGKILL ~450 ms after that, all pid-targeted at the server rather than sent to its group.
/// So the loop above does not end — the process is killed inside it. The hold is kept because it is
/// correct for every client marion itself writes (`marion run`, the tests, a future TUI), and the
/// gap it leaves is §11 item 30: a backgrounded child outlives the SIGKILL as an untracked process
/// reparented to pid 1, which is what §9's *"no untracked live process"* forbids.
///
/// **§11 item 30 lists six journal shapes that kill can leave, not one**, because it can land
/// anywhere in `run_spawn`'s timeline and that timeline separates intent, confirmation and
/// persistence. Nothing in this function recovers any of them, and nothing pretends to: a restarted
/// bridge starts with an empty `Background` table and no pid, so a survivor cannot be re-associated
/// with the handle this process handed out.
fn run_bridge() {
    let bg = marion_supervisor::background::Background::new();
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
    // EOF. Every child this bridge started and nobody collected is finished here, not abandoned.
    bg.join_all();
}

#[cfg(test)]
mod main_tests {
    use super::*;

    #[test]
    fn task_ids_minted_back_to_back_use_entropy_and_do_not_collide() {
        assert_ne!(new_task_id().unwrap(), new_task_id().unwrap());
    }

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

    /// The gate's inputs come off the declaration marion wrote, and both are load-bearing: without
    /// the type there is no `max_depth` to read, and without the depth there is nothing to compare
    /// it against.
    #[test]
    fn the_callers_type_and_depth_are_read_off_the_declaration() {
        let c = caller_from("019f-node", Some("codex".into()), Some("2".into()))
            .expect("a declaration carrying both resolves");
        assert_eq!(c.agent_id, "019f-node");
        assert_eq!(
            c.agent_type.name, "codex-impl",
            "the alias resolves to the one definition, not a second one"
        );
        assert_eq!(c.depth, 2);
        assert_eq!(
            c.agent_type.max_depth, 3,
            "which is the bound the gate reads"
        );
    }

    /// **A bridge that was not told is a refusal, not a default.** Both plausible defaults restore
    /// the unbounded recursion this exists to stop: depth 0 makes every node look like a root, and
    /// a constant `max_depth` makes the per-type key a number nothing reads. The message has to say
    /// which piece is missing, because the operator's fix differs.
    #[test]
    fn a_bridge_that_was_not_told_which_node_it_serves_refuses_rather_than_spawning_ungated() {
        for (agent_type, depth, expected) in [
            (None, Some("0".to_string()), "MARION_AGENT_TYPE is not set"),
            (Some("claude".to_string()), None, "MARION_DEPTH is not set"),
            (
                Some("not-a-type".to_string()),
                Some("0".to_string()),
                "names no known agent type",
            ),
            (
                Some("claude".to_string()),
                Some("deep".to_string()),
                "is not a depth",
            ),
        ] {
            let e = caller_from("019f-node", agent_type.clone(), depth.clone())
                .expect_err("an unevaluable gate must refuse");
            assert!(e.contains(expected), "expected {expected:?} in: {e}");
            assert!(
                e.contains("ungated"),
                "the refusal must say what it is protecting against: {e}"
            );
        }
    }

    /// **An absent `MARION_AUTH` is `Canned`, and the fallback is stated rather than inferred.**
    ///
    /// This is the one declaration key that falls back silently where its siblings refuse:
    /// `caller_from` turns a missing `MARION_AGENT_TYPE` or `MARION_DEPTH` into an error, because
    /// both plausible defaults there restore the unbounded recursion the gate exists to stop. Here
    /// the cheap direction is the safe one — every declaration written before this key existed
    /// meant canned — so the fallback is deliberate, and a silent default that nothing pins is a
    /// default that can drift into the expensive direction without a test noticing.
    ///
    /// The endpoint is asserted alongside the mode because a canned node with no endpoint is not
    /// canned in any usable sense: it is the third state `auth_from_env` exists to make impossible.
    #[test]
    fn an_absent_auth_key_is_canned_rather_than_a_guess_at_live() {
        let (auth, base_url) = auth_from_env(None, None);
        assert_eq!(
            auth,
            marion_harness::Auth::Canned,
            "a declaration written before MARION_AUTH existed meant canned; inferring live from an \
             absent key points a real credential somewhere marion did not choose"
        );
        assert_eq!(
            base_url.as_deref(),
            Some("http://127.0.0.1:8099/v1"),
            "a canned node with no endpoint is neither canned nor live"
        );
    }

    /// **An unrecognised `MARION_AUTH` is `Canned` too, not a guess in the expensive direction.**
    ///
    /// [`marion_harness::Auth::from_wire`] returns `None` for anything it does not know, and the
    /// asymmetry is the whole point: guessing canned costs a run against an endpoint that is not
    /// listening — loud, local, free — while guessing live spends the operator's credential on a
    /// value marion could not even parse. A typo in a declaration must therefore fail cheap.
    #[test]
    fn an_unrecognised_auth_value_is_canned_rather_than_a_guess_at_live() {
        let (auth, base_url) = auth_from_env(Some("Inherited".into()), None);
        assert_eq!(
            auth,
            marion_harness::Auth::Canned,
            "from_wire is exact: a near-miss spelling must fail toward the cheap error, not spend \
             a real credential"
        );
        assert_eq!(base_url.as_deref(), Some("http://127.0.0.1:8099/v1"));
    }

    /// **A live root's child is live.** The reading half of the hop; the writing half is
    /// `tests/auth_mode.rs`, which pins that all four adapters put this exact value into the
    /// declaration this reads back. Nothing launches.
    ///
    /// **The input is `as_wire`, not a typed `"inherited"`**, and that is what keeps the two halves
    /// from drifting: both ends take the token from the one function every adapter serialises
    /// through, so a rename cannot leave one side passing against a stale literal. The tables are
    /// tied to each other by `the_wire_spelling_round_trips_so_the_two_halves_cannot_drift_apart`
    /// over in that file — `as_wire` and `from_wire` are two independent `match`es.
    ///
    /// The endpoint is `None` and that is the point: live means marion overlays no endpoint on the
    /// child, exactly as it overlays none on the root. A canned child of a live root would launch
    /// against marion's canned server, which under real auth is not running.
    #[test]
    fn a_declaration_saying_inherited_makes_the_child_live_and_names_it_no_endpoint() {
        let (auth, base_url) = auth_from_env(
            Some(marion_harness::Auth::Inherited.as_wire().to_string()),
            None,
        );
        assert_eq!(auth, marion_harness::Auth::Inherited);
        assert_eq!(
            base_url, None,
            "a live child is overlaid no endpoint, exactly as `marion run` overlays none on a live \
             root"
        );
    }

    /// **The mode is a stated decision, never inferred from a URL.**
    ///
    /// An `Inherited` run can legitimately carry a base URL — a proxy or a gateway is a reasonable
    /// thing to point a real credential at, and `resolve_base_url` accepts a non-loopback one. So a
    /// carried endpoint must not demote the child to canned: that is the "endpoint-as-mode
    /// conflation" the design names, and inferring the mode from the URL would send a child that
    /// was declared live to marion's canned server instead.
    #[test]
    fn a_carried_base_url_does_not_demote_a_child_that_was_declared_live() {
        let (auth, base_url) = auth_from_env(
            Some(marion_harness::Auth::Inherited.as_wire().to_string()),
            Some("https://gateway.corp.example/v1".into()),
        );
        assert_eq!(
            auth,
            marion_harness::Auth::Inherited,
            "the declaration said inherited; an endpoint beside it is not a second opinion on the \
             mode"
        );
        assert_eq!(
            base_url, None,
            "and the endpoint is ignored rather than obeyed — see tests/auth_mode.rs, which pins \
             that every adapter drops it too"
        );
    }

    /// **An empty `MARION_BASE_URL` is absent, not an endpoint.** The measured regression, not a
    /// hypothetical: `unwrap_or_default()` wrote `MARION_BASE_URL: ""` into a live root's
    /// declaration, this function's caller read it back as `Ok("")`, and the child was compiled
    /// canned against an endpoint spelled as the empty string — neither live nor working, with
    /// nothing anywhere reporting it. A canned child gets marion's own provider instead.
    #[test]
    fn an_empty_base_url_falls_back_to_the_canned_endpoint_rather_than_being_obeyed() {
        let (auth, base_url) = auth_from_env(
            Some(marion_harness::Auth::Canned.as_wire().to_string()),
            Some("   ".into()),
        );
        assert_eq!(auth, marion_harness::Auth::Canned);
        assert_eq!(
            base_url.as_deref(),
            Some("http://127.0.0.1:8099/v1"),
            "a blank endpoint is a third state — neither live nor canned — and this is where it is \
             made impossible"
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
