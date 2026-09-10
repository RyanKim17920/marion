//! **§7.6's descendant gating, end to end: a real `codex` child stops while a grandchild it
//! backgrounded is still running.**
//!
//! Design §7.6 states the rule and its two exemptions, and principle 11 of `MILESTONES.md` calls it
//! non-negotiable:
//!
//! > A node's `Exited` is **held** while any descendant is non-terminal — *unless* the node reported
//! > early or the hold bound expired.
//!
//! Three runs, one per branch of that sentence, each driving the **real** binary through marion's
//! canned provider so that the stop marion gates is a real harness's stop and not a fixture's:
//!
//! 1. **Reported early.** The child backgrounds a grandchild, calls `report`, and ends its turn.
//!    Its exit is accepted at once — a staged report is a deliberate conclusion — and the contract
//!    says so: `reported_early: true`, with the still-live grandchild named in
//!    `live_descendants_at_report`.
//! 2. **Held, then released.** The child backgrounds a grandchild and ends its turn **without**
//!    reporting. marion holds the node in `Blocked(Descendants)` — observable in the journal, which
//!    is what this test waits on — and writes its `Exited` only after the grandchild's own. The
//!    contract is `Unreported`, neither flag set, and its exit description names the rule.
//! 3. **Held to the bound.** As 2, with a short `timeout_secs`. The bound expires first, so the
//!    node terminates `held_to_timeout: true` with the grandchild listed — and the grandchild
//!    **outlives** it (§7.5: killing it would destroy work to tidy up bookkeeping).
//!
//! # The bed
//!
//! `depth_gate.rs`'s, reused: a detached supervisor with a `codex` shim ahead of the real binary on
//! its `PATH`. The root and the grandchild are shims blocked on gate files the test holds, so they
//! are real processes marion really owns and cost no model call; the one invocation that must be
//! real — the child under test — is told apart by [`DELEGATOR_MARKER`] in its argv, and the shim
//! `exec`s the real `codex` for it. Every condition waited on below is a **fact in the journal**,
//! never an elapsed time.
//!
//! ```sh
//! cargo test -p marion-supervisor --test descendant_gate
//! ```
//!
//! It needs a real `codex` on `PATH` and, like every end-to-end file here, does not skip when it is
//! missing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_core::node::{BlockReason, NodeState};
use marion_core::paths::ProjectDir;
use marion_core::registry::Replay;
use marion_proto::params::AgentSpawnParams;
use marion_proto::{Call, Method, MethodResult, SpawnCaller};
use marion_provider::{CannedServer, Config, NodeScript, Script, ScriptedCall};
use marion_supervisor::journal::read_path;
use marion_supervisor::socket::project_root;
use marion_testsupport::{
    fixture_repo, kill_hard, on_path, persisted_contracts, pinned_version, scratch, survivors,
};
use serde_json::{Value, json};

mod common;

/// Present in the **child under test's** prompt and nowhere else: the shim routes on it to the real
/// binary, and the provider dispatches on it to the child's script.
const DELEGATOR_MARKER: &str = "MARION-DESCENDANT-GATE-DELEGATOR-4e7b";

/// Present in the **root's** prompt and nowhere else, so the shim can tell the one invocation that
/// must outlive the whole test from the grandchild, which must outlive only the child.
const ROOT_MARKER: &str = "MARION-DESCENDANT-GATE-ROOT-4e7b";

/// The grandchild's task. Carries neither marker: the shim blocks it on the grandchild gate.
const GRANDCHILD_PROMPT: &str = "descendant-gate grandchild: hold until released";

/// What the child reports in run 1.
const CHILD_NARRATIVE: &str =
    "Delegated the follow-up to a grandchild and reporting with what I have.";

/// The child's bound where the bound must **not** be reached: generous, ended by the script.
const CHILD_TIMEOUT_SECS: u64 = 120;

/// The child's bound where the bound **must** be reached first (run 3). The child's own turn takes
/// a few seconds against the canned provider; the remainder is the hold this run measures the
/// expiry of. Short so the suite is not slow, long enough that a slow machine's turn cannot eat it
/// whole and turn a `held_to_timeout` into a `TimedOut`.
const SHORT_CHILD_TIMEOUT_SECS: u64 = 25;

/// The wall clock the shims run under. Never reached: the gates end them.
const SHIM_TIMEOUT_SECS: u64 = 600;

/// §5.7's idle grace for the fixture's supervisor. Never waited out — a live root keeps it resident
/// and the fixture ends it explicitly.
const IDLE_GRACE: Duration = Duration::from_secs(600);

/// A bound that exists only to fail. Nothing here waits on work the test has not already caused,
/// except run 3, whose wait is the child's own [`SHORT_CHILD_TIMEOUT_SECS`].
const BOUND: Duration = Duration::from_secs(180);

const CHILD_CALL_PREFIX: &str = "call_descendant_gate_child";

/// The child's script: background one grandchild, then either report or simply stop.
fn child_script(reports: bool) -> Script {
    let mut turns = vec![ScriptedCall::new(
        "spawn",
        json!({
            "agent_type": "codex-impl",
            "prompt": GRANDCHILD_PROMPT,
            "acceptance_criteria": ["the grandchild was released"],
            "writable_scope": ["src/**"],
            "timeout_secs": SHIM_TIMEOUT_SECS,
            "background": true,
        }),
    )];
    if reports {
        turns.push(ScriptedCall::new(
            "report",
            json!({"narrative": CHILD_NARRATIVE}),
        ));
    }
    Script {
        nodes: vec![NodeScript {
            marker: DELEGATOR_MARKER.into(),
            call_prefix: CHILD_CALL_PREFIX.into(),
            turns,
            final_text: "Backgrounded a grandchild; ending my turn.".into(),
        }],
        ..Script::default()
    }
}

/// The shim: a `codex` that `exec`s the real binary for the child under test and blocks every other
/// invocation on the gate its marker selects. Mortal by construction.
fn shim(dir: &Path, root_gate: &Path, grandchild_gate: &Path, real: &Path) -> PathBuf {
    let bin = dir.join("codex");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --version) echo "codex-cli 0.146.0-marion-descendant-shim"; exit 0 ;;
esac
case "$*" in
  *{delegator}*) exec {real} "$@" ;;
  *{root}*) gate={root_gate} ;;
  *) gate={grandchild_gate} ;;
esac
waited=0
while [ ! -e "$gate" ]; do
  sleep 0.05
  waited=$((waited + 1))
  if [ "$waited" -gt 4000 ]; then exit 0; fi
done
exit 0
"#,
        delegator = DELEGATOR_MARKER,
        root = ROOT_MARKER,
        real = shell_quote(real),
        root_gate = shell_quote(root_gate),
        grandchild_gate = shell_quote(grandchild_gate),
    );
    std::fs::write(&bin, script).expect("the shim is written");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("the shim is executable");
    bin
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

fn which(program: &str) -> PathBuf {
    let out = std::process::Command::new("which")
        .arg(program)
        .output()
        .expect("`which` runs");
    assert!(
        out.status.success(),
        "this test drives a REAL {program}; put it on PATH"
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// A node the supervisor owns, and the capability its own bridge would present.
struct Owned {
    agent_id: AgentId,
    token: String,
}

fn spawn_over_socket(
    sup: &common::Supervisor,
    state: &Path,
    caller: Option<&Owned>,
    p: AgentSpawnParams,
) -> Owned {
    let p = AgentSpawnParams {
        native_launch: None,
        caller: caller.map(|c| SpawnCaller {
            agent_id: c.agent_id.clone(),
            node_token: c.token.clone(),
        }),
        ..p
    };
    let answered = sup
        .call(Call::AgentSpawn(p))
        .unwrap_or_else(|e| panic!("agent/spawn must be served: {e}"));
    let MethodResult::AgentSpawn(r) = Method::AgentSpawn
        .decode_result(&answered)
        .expect("a well-formed agent/spawn result")
    else {
        panic!("agent/spawn answers with an agent/spawn result");
    };
    let token = common::declaration_of(state, &r.agent_id)
        .remove("MARION_NODE_TOKEN")
        .expect("declaration_of asserts the token is there");
    Owned {
        agent_id: r.agent_id,
        token,
    }
}

fn params(prompt: String, repo: Option<&Path>, timeout_secs: u64) -> AgentSpawnParams {
    AgentSpawnParams {
        agent_type: "codex-impl".into(),
        prompt,
        native_launch: None,
        caller: None,
        repo: repo.map(Path::to_path_buf),
        acceptance_criteria: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: Some(timeout_secs),
        model: None,
        no_change_record: repo.map(|_| true),
        pane: None,
        isolation: None,
        allow_concurrent_writes: None,
    }
}

// ---- reading the journal back -------------------------------------------------------------------

fn tree(journal: &Path) -> Replay {
    read_path(journal).expect("the journal reads back")
}

fn state_of(journal: &Path, id: &AgentId) -> Option<NodeState> {
    tree(journal).get(id).map(|n| n.state)
}

fn is_exited(journal: &Path, id: &AgentId) -> bool {
    state_of(journal, id).is_some_and(|s| s.is_exited())
}

/// Poll a journal fact under [`BOUND`]. The fact, never the time, is what is asserted.
fn wait_for(journal: &Path, what: &str, mut fact: impl FnMut(&Path) -> bool) {
    let deadline = Instant::now() + BOUND;
    while !fact(journal) {
        assert!(
            Instant::now() < deadline,
            "{what} never became true within {BOUND:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for §7.6 step 3's hold — the node journaled `Blocked(Descendants)` — and fail **by name**
/// if its `Exited` lands first while the grandchild is still live, which is the defect this file
/// exists to close: an exit that was never held.
fn wait_for_hold(journal: &Path, child: &AgentId, grandchild: &AgentId) {
    wait_for(journal, "the child's Blocked(Descendants)", |j| {
        assert!(
            !is_exited(j, child) || is_exited(j, grandchild),
            "§7.6: the child's Exited landed while its grandchild {} was live, and no hold was \
             journaled — its state is {:?}. The exit was never gated.",
            grandchild.0,
            state_of(j, child)
        );
        state_of(j, child) == Some(NodeState::Blocked(BlockReason::Descendants))
    });
}

/// The one child the journal records under `parent`.
fn only_child_of(journal: &Path, parent: &AgentId) -> AgentId {
    let t = tree(journal);
    let kids = t.children(parent);
    assert_eq!(
        kids.len(),
        1,
        "exactly one grandchild was asked for; the journal records {} under {}",
        kids.len(),
        parent.0
    );
    kids[0].agent_id.clone()
}

/// Every journal line, in the file's byte order — the journal's total order.
fn journal_lines(journal: &Path) -> Vec<Value> {
    std::fs::read_to_string(journal)
        .expect("the journal reads back")
        .lines()
        .map(|l| serde_json::from_str(l).expect("a journal line is JSON"))
        .collect()
}

fn only_index(records: &[Value], kind: &str, agent_id: &AgentId) -> usize {
    let hits: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.pointer(&format!("/kind/{kind}/agent_id"))
                .and_then(Value::as_str)
                == Some(agent_id.0.as_str())
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one {kind} about {}, found {hits:?}",
        agent_id.0
    );
    hits[0]
}

/// The contract marion persisted for `id`, parsed.
fn contract_of(state: &Path, id: &AgentId) -> Value {
    let all = persisted_contracts(state).expect("the state tree walks");
    let mine: Vec<&marion_testsupport::PersistedContract> = all
        .iter()
        .filter(|c| c.path.to_string_lossy().contains(&id.0))
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "one contract for the child under test; found {:?}",
        mine.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
    mine[0]
        .parsed
        .clone()
        .unwrap_or_else(|e| panic!("the child's contract does not read back: {e}"))
}

// ---- the bed --------------------------------------------------------------------------------------

struct Bed {
    dir: marion_testsupport::Scratch,
    state: PathBuf,
    journal: PathBuf,
    root_gate: PathBuf,
    grandchild_gate: PathBuf,
    server: Option<CannedServer>,
    sup: common::Supervisor,
    child: Owned,
}

impl Bed {
    fn start(tag: &str, reports: bool, child_timeout_secs: u64) -> Bed {
        assert!(
            on_path("codex"),
            "this test drives a REAL codex child; put `codex` ({}) on PATH",
            pinned_version("codex")
        );
        let dir = scratch(&format!("descendant-gate-{tag}"));
        let repo = fixture_repo(&dir);
        let state = dir.join("state");
        let shim_dir = dir.join("bin");
        let root_gate = dir.join("root-gate");
        let grandchild_gate = dir.join("grandchild-gate");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&shim_dir).unwrap();
        shim(&shim_dir, &root_gate, &grandchild_gate, &which("codex"));

        let server = CannedServer::start(Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: child_script(reports),
        })
        .expect("the canned provider binds");

        let path_env = format!(
            "{}:{}",
            shim_dir.to_string_lossy(),
            std::env::var("PATH").unwrap_or_default()
        );
        let key = project_root(&repo);
        let project = ProjectDir::new(&state, &key);
        let sup =
            common::Supervisor::start(&state, &key, &path_env, &server.base_url(), IDLE_GRACE);

        let root = spawn_over_socket(
            &sup,
            &state,
            None,
            params(
                format!("{ROOT_MARKER}: hold the tree open"),
                Some(&repo),
                SHIM_TIMEOUT_SECS,
            ),
        );
        let child = spawn_over_socket(
            &sup,
            &state,
            Some(&root),
            params(
                format!("{DELEGATOR_MARKER}: background a grandchild, then stop."),
                None,
                child_timeout_secs,
            ),
        );
        Bed {
            journal: project.journal(),
            dir,
            state,
            root_gate,
            grandchild_gate,
            server: Some(server),
            sup,
            child,
        }
    }

    /// The grandchild the child under test backgrounded, once the journal has its intent.
    fn grandchild(&self) -> AgentId {
        let child = self.child.agent_id.clone();
        wait_for(&self.journal, "the grandchild's SpawnIntent", |j| {
            !tree(j).children(&child).is_empty()
        });
        only_child_of(&self.journal, &child)
    }

    fn release_grandchild(&self) {
        std::fs::write(&self.grandchild_gate, b"go").expect("the grandchild is released");
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        // Release every shim first: the supervisor does not signal a node's process group when it
        // dies, so killing it first would strand every blocked shim on its own life cap.
        let _ = std::fs::write(&self.grandchild_gate, b"go");
        let _ = std::fs::write(&self.root_gate, b"go");
        let needle = self.dir.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !survivors(&needle).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.sup.stop();
        drop(self.server.take());
        for (pid, _) in survivors(&needle) {
            kill_hard(pid);
        }
    }
}

// ---- the three branches ---------------------------------------------------------------------------

#[test]
fn a_codex_child_that_reports_with_a_live_grandchild_exits_reported_early_naming_it() {
    let bed = Bed::start("reported-early", true, CHILD_TIMEOUT_SECS);
    let child = bed.child.agent_id.clone();
    let grandchild = bed.grandchild();

    wait_for(&bed.journal, "the child's Exited", |j| is_exited(j, &child));
    assert!(
        !is_exited(&bed.journal, &grandchild),
        "the grandchild is a shim on a gate this test has not opened; it cannot have exited"
    );

    let c = contract_of(&bed.state, &child);
    let completion = &c["completion"];
    assert_eq!(completion["status"], json!("Ok"), "{completion}");
    assert_eq!(
        completion["reported_early"],
        json!(true),
        "§7.6: a node that delivers a `report` with a live descendant concluded on purpose, and the \
         contract must say so rather than read as an ordinary finish — {completion}"
    );
    assert_eq!(
        completion["live_descendants_at_report"],
        json!([grandchild.0]),
        "§7.6: the still-running descendants are listed, so a reader can tell \"done\" from \"done \
         for now\" — {completion}"
    );
    assert_eq!(completion["held_to_timeout"], json!(false), "{completion}");
    assert_eq!(completion["died_before_gate"], json!(false), "{completion}");
}

#[test]
fn a_codex_child_that_stops_unreported_with_a_live_grandchild_is_held_until_it_is_terminal() {
    let bed = Bed::start("held-released", false, CHILD_TIMEOUT_SECS);
    let child = bed.child.agent_id.clone();
    let grandchild = bed.grandchild();

    // The hold is a journal fact: §7.6 step 3 holds the node in `Blocked`. This is the production
    // barrier the test waits on — never the child's process, whose death is unordered with respect
    // to marion's bookkeeping.
    wait_for_hold(&bed.journal, &child, &grandchild);
    assert!(
        !is_exited(&bed.journal, &child),
        "the child stopped with a live grandchild and no report; §7.6 holds its Exited"
    );

    bed.release_grandchild();
    wait_for(&bed.journal, "the child's Exited", |j| is_exited(j, &child));
    assert!(is_exited(&bed.journal, &grandchild));

    let records = journal_lines(&bed.journal);
    assert!(
        only_index(&records, "Exited", &grandchild) < only_index(&records, "Exited", &child),
        "the journal's byte order is its total order, and the grandchild's terminal must precede \
         the child's: the child was released *by* the grandchild finishing"
    );

    let c = contract_of(&bed.state, &child);
    let completion = &c["completion"];
    assert_eq!(completion["status"], json!("Unreported"), "{completion}");
    assert_eq!(completion["reported_early"], json!(false), "{completion}");
    assert_eq!(completion["held_to_timeout"], json!(false), "{completion}");
    let description = completion["exit"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        description.contains("descendant") && description.contains("§7.6"),
        "the exit description must name the rule that held the node: {description:?}"
    );
}

#[test]
fn a_codex_child_held_to_its_bound_exits_held_to_timeout_and_its_grandchild_outlives_it() {
    let bed = Bed::start("held-to-timeout", false, SHORT_CHILD_TIMEOUT_SECS);
    let child = bed.child.agent_id.clone();
    let grandchild = bed.grandchild();

    wait_for_hold(&bed.journal, &child, &grandchild);
    wait_for(&bed.journal, "the child's Exited", |j| is_exited(j, &child));
    assert!(
        !is_exited(&bed.journal, &grandchild),
        "§7.6/§7.5: the still-running descendants outlive the parent; killing them would destroy \
         work to tidy up bookkeeping"
    );

    let c = contract_of(&bed.state, &child);
    let completion = &c["completion"];
    assert_eq!(
        completion["status"],
        json!("Unreported"),
        "§7.6 step 3: a task node whose hold bound expires becomes Exited{{Unreported}} — {completion}"
    );
    assert_eq!(
        completion["held_to_timeout"],
        json!(true),
        "the flag is what keeps an Unreported exit with a live descendant legal under L1 — \
         {completion}"
    );
    assert_eq!(completion["reported_early"], json!(false), "{completion}");
    assert_eq!(
        completion["live_descendants_at_report"],
        json!([grandchild.0]),
        "the set at the moment the flag was set — {completion}"
    );
    let description = completion["exit"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        description.contains("descendant") && description.contains("§7.6"),
        "the exit description must name the rule that held the node: {description:?}"
    );
}
