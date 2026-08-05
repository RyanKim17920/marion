//! `marion-supervisor` — the supervisor binary.
//!
//! `mcp` and `doctor` are **subcommands of this binary**, not separate ones (§10). The
//! user-facing `marion` command forwards `doctor` here, since the supervisor owns the registry
//! the check reads.

use std::io::{BufRead, Read, Write};

use marion_core::ids::{RAND_BYTES, new_task_id as core_new_task_id};
use marion_core::paths::{ProjectDir, state_dir};
use marion_supervisor::root::{AGENT_ID_ENV, AGENT_TYPE_ENV, DEPTH_ENV, READY_FILE_ENV};
use marion_supervisor::{bridge, run, spawn};

fn usage() -> ! {
    eprintln!("usage: marion-supervisor <mcp|doctor>");
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("mcp") => run_bridge(),
        Some("doctor") => {
            println!("marion doctor: no adapters registered yet (M1 in progress)");
        }
        _ => usage(),
    }
}

/// `spawn` blocks for the child's whole run and returns the completed contract; `report` stages
/// the child's payload, which the parent's own `spawn` then returns.
fn handle_tool_call(
    id: &serde_json::Value,
    name: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
    match name {
        "report" => match report_refusal(std::env::var(DEPTH_ENV).ok()) {
            // §5.4, at the execution point. See [`report_refusal`].
            Some(msg) => bridge::tool_result(id, msg, true),
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
            let Ok(env) = spawn_env() else {
                return bridge::tool_result(id, "marion: MARION_REPO is not set", true);
            };
            // §6.1 step 2's gates read the caller's agent type and depth, and this bridge is the
            // only place that knows which node it is serving. A bridge that was not told cannot
            // evaluate them, so it refuses rather than spawning ungated — which is exactly the
            // hazard this path exists to close, and the same shape as `spawn_env`'s refusal above.
            let caller = match caller(&requester()) {
                Ok(c) => c,
                Err(e) => return bridge::tool_result(id, &e, true),
            };
            let req = run::SpawnRequest {
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
            // A child that ran and failed and a spawn that never launched are the same news to
            // the parent, and used to arrive in two different shapes. `bridge::spawn_result` is
            // where that is decided, in one place, so both read alike.
            bridge::spawn_result(
                id,
                &req.agent_type,
                run::run_spawn(&env, &req, &task_id, &caller),
            )
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
/// **An unreadable depth is not a refusal here, and that is the opposite call from [`caller_from`]
/// deliberately.** There, both plausible defaults restore the unbounded recursion the gate exists to
/// stop, and the thing being refused creates a process tree and a worktree. Here nothing is created
/// either way — a `report` marion declines to stage has no side effect to prevent — and the refusal
/// is a *claim about this node* ("you are the root and have no contract") that marion would have no
/// grounds to make. All four adapters emit `MARION_DEPTH`, swept by `marion_harness::adapter`'s own
/// test, so the only caller that can land here is a hand-started bridge.
fn report_refusal(depth: Option<String>) -> Option<&'static str> {
    let depth: u32 = depth?.trim().parse().ok()?;
    bridge::authorization_refusal(depth, bridge::REPORT)
}

/// **A `spawn` parameter marion declares and does not implement, if this request carries one.**
///
/// §5.4's schema has eleven keys and `run::SpawnRequest` carries six. The five that were never read
/// split cleanly in two, and only one half belongs here:
///
/// * **A different verb performed quietly** — `background`, `isolation` and `verification`. Each
///   made marion do something other than what was asked while answering `isError: false`, which is
///   the §12 accept-and-ignore shape (`default_tools_approval_mode`, `trust: true`). Refused, by
///   name, with the value that broke it; see each [`spawn::SpawnError`] variant for its own reason.
/// * **Simply absent** — `name` and `allow_concurrent_writes`. Nothing consumes them and nothing
///   contradicts them: `TaskContract` has no `name` field and no verb addresses a node by one, and
///   `allow_concurrent_writes` is §6.6's escape hatch from a shared-cwd write-conflict refusal that
///   is not in code, for a `shared-cwd` mode the refusal above now makes unreachable. Dropping
///   either changes no answer any caller receives, so they are left accepted and recorded in §11
///   item 23 rather than refused — a refusal there would cost callers a working spawn and buy no
///   honesty.
///
/// **The permitted values are not refused**, which is the whole point of reading the field rather
/// than rejecting its presence: `background: false`, `isolation: "worktree"` and an empty
/// `verification` all describe exactly what marion does, and an absent key asks for nothing.
///
/// Pure, and separate from [`handle_tool_call`], so the table above is testable as a table.
fn unimplemented_parameter(args: &serde_json::Value) -> Option<spawn::SpawnError> {
    if args["background"].as_bool() == Some(true) {
        return Some(spawn::SpawnError::BackgroundUnimplemented);
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
    let raw = depth.ok_or_else(|| {
        format!(
            "marion: {DEPTH_ENV} is not set, so this bridge does not know how deep in the tree it \
             is and cannot enforce max_depth (§6.1 step 2). Refusing rather than spawning ungated."
        )
    })?;
    let depth: u32 = raw.trim().parse().map_err(|_| {
        format!(
            "marion: {DEPTH_ENV}={raw:?} is not a depth. Refusing rather than spawning ungated."
        )
    })?;
    Ok(run::Caller {
        agent_id: agent_id.to_string(),
        agent_type,
        depth,
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

fn spawn_env() -> Result<run::Env, ()> {
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
    Ok(run::Env {
        project_dir: ProjectDir::new(&state, &repo),
        repo,
        bridge: std::env::current_exe().unwrap_or_else(|_| "marion-supervisor".into()),
        base_url,
        auth,
    })
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

/// Serve MCP over stdio until the harness closes our stdin.
fn run_bridge() {
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
            } => Some(handle_tool_call(&id, &name, &arguments)),
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
}

#[cfg(test)]
mod main_tests {
    use super::*;

    #[test]
    fn task_ids_minted_back_to_back_use_entropy_and_do_not_collide() {
        assert_ne!(new_task_id().unwrap(), new_task_id().unwrap());
    }

    /// **A declared parameter marion does not implement is refused, not quietly performed as
    /// something else.**
    ///
    /// `background` is in `bridge::tools`' `spawn` schema and §5.4 pins it: `"background": false //
    /// M1: must be false (§9)`. Before this refusal the field was never read anywhere — a caller
    /// asking for a handle got a **completed contract** back with `isError: false`, having waited
    /// out the child's whole run. That is not a failure it can detect and not the verb it asked
    /// for: a parent backgrounding four children to get concurrency got four serialized ones and
    /// no signal anywhere. It is the §12 accept-and-ignore shape, in the same family as
    /// `default_tools_approval_mode` and `trust: true`.
    ///
    /// Three assertions, each of which fails against the pre-fix code:
    ///
    /// 1. it is an **error result** at all — pre-fix this call ran a child and returned its
    ///    contract with `isError: false`;
    /// 2. the message **names the field and says unimplemented**, so the caller can tell "marion
    ///    will not" from "marion could not";
    /// 3. it is refused **without an environment**, which is how we know nothing was attempted:
    ///    this test sets no `MARION_REPO` and no agent declaration, so a `spawn` that got past the
    ///    refusal could not even build an `Env` — the two later refusals would answer instead, and
    ///    neither mentions `background`.
    #[test]
    fn a_backgrounded_spawn_is_refused_by_name_rather_than_served_synchronously() {
        let v = handle_tool_call(
            &serde_json::json!(1),
            "spawn",
            &serde_json::json!({
                "agent_type": "codex-impl",
                "prompt": "do the task",
                "acceptance_criteria": [],
                "background": true
            }),
        );

        assert_eq!(
            v["result"]["isError"],
            serde_json::json!(true),
            "a spawn marion will not perform is an error result, not a contract: {v}"
        );
        let text = v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            text.contains("background") && text.contains("not implemented"),
            "the refusal must name the field and say it is unimplemented, got: {text}"
        );
        assert!(
            !text.contains("MARION_REPO"),
            "and it must precede the environment checks, so nothing was attempted: {text}"
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
                "background",
                serde_json::json!({"background": true}),
                "background",
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
            unimplemented_parameter(&serde_json::json!({"background": true})),
            Some(BackgroundUnimplemented)
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
                "the two dropped-but-harmless keys",
                serde_json::json!({"name": "impl-auth", "allow_concurrent_writes": true}),
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
    /// `LaunchOnly` node's MCP client does. This is the decision underneath it, including the two
    /// rows that test cannot reach: a bridge that was told a depth it cannot read, and one that was
    /// told nothing at all. Both serve rather than refuse — see [`report_refusal`] for why that is
    /// the opposite call from `caller_from`'s, on purpose.
    #[test]
    fn only_a_node_that_is_known_to_be_the_root_has_its_report_refused() {
        assert_eq!(
            report_refusal(Some("0".into())),
            Some(bridge::REPORT_ON_A_ROOT),
            "§5.4 rejects `report` on a root, and depth 0 is what being the root means (§3.1)"
        );
        for (label, depth) in [
            ("a child", Some("1".to_string())),
            ("a grandchild", Some("2".to_string())),
            // Whitespace survives an env round trip on some shells; the depth is the number.
            ("a padded depth", Some(" 1 ".to_string())),
            ("a depth that is not a number", Some("deep".to_string())),
            ("a bridge that was told nothing", None),
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
