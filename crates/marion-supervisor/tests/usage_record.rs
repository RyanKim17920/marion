//! **A node's token usage, recorded where it outlives the node: its contract and the journal.**
//!
//! The harness rows read a run's usage off its stream, and until this was wired nothing kept the
//! figure — the contract had no field for it and the journal no record, so a view re-read the
//! node's whole `events.jsonl`, and again after every restart. One real codex child through the
//! canned provider, whose every response reports a fixed spend
//! ([`marion_provider::USAGE`]), measures the three places the figure must land:
//!
//! 1. the contract `run_spawn` returns, and the copy it persisted;
//! 2. exactly one `UsageRecorded` journal record for the node, carrying the same figure;
//! 3. a fresh replay of the journal file — what a restarted supervisor folds — which puts the same
//!    figure on the node with no read of its stream at all.
//!
//! The canned spend is distinct per counter, so each counter is checked to have arrived in its own
//! field (codex counts cache reads inside its input, which the row takes back out, and reasoning
//! inside its output, which the row names). The figure is `n` times the canned one for the `n`
//! responses the child's turn took, so `n` is read off the output and every other counter is held
//! to it.
//!
//! ```sh
//! cargo test -p marion-supervisor --test usage_record
//! ```
//!
//! It needs a real `codex` on `PATH`. Every model call is the canned server's: no paid tokens.

use std::path::Path;

use marion_core::contract::{AgentId, Isolation, TaskContract, TaskId, TokenUsage};
use marion_core::journal::{RecordKind, decode};
use marion_provider::USAGE;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::journal::read_path;
use marion_supervisor::run::{Caller, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, harness_available, persisted_contracts, scratch};

mod common;

use common::canned::canned_env;

const NARRATIVE: &str = "Edited the file under src/ and reported back.";
const PATCH: &str = "*** Begin Patch\n*** Update File: src/keep.txt\n@@\n-keep\n\
                     +keep, edited by the canned codex child\n*** End Patch";

/// The figure `n` canned responses add up to under codex's row: its input counts the cache
/// reads, which the row takes back out, and its output counts the reasoning, which the row names.
fn canned_times(n: u64) -> TokenUsage {
    TokenUsage {
        input: n * (USAGE.input - USAGE.cached),
        output: n * USAGE.output,
        cache_read: n * USAGE.cached,
        cache_write: 0,
        reasoning: Some(n * USAGE.reasoning),
    }
}

#[test]
fn a_childs_usage_lands_in_its_contract_and_one_journal_record_and_survives_replay() {
    if !harness_available("codex") {
        return;
    }
    let root = scratch("usage-record");
    let repo = fixture_repo(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root.join("provider-requests.jsonl"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            child_patch: PATCH.into(),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let env = canned_env(&state, &repo, Some(server.base_url()));
    let task = TaskId("usage-1".into());
    let contract = run_spawn(
        &env,
        &SpawnRequest {
            review: None,
            agent_type: "codex-impl".into(),
            prompt: "Edit the file under src/ and report back through marion.".into(),
            repo: repo.clone(),
            acceptance_criteria: vec!["a file under src/ was edited".into()],
            verification: vec![],
            writable_scope: vec!["src/**".into()],
            timeout_secs: 60,
            model: None,
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
            race: None,
        },
        &task,
        &Caller::root(
            "root",
            marion_core::agent_type::builtin("claude").expect("the root type resolves"),
        ),
    )
    .unwrap_or_else(|e| panic!("the child runs: {e}"));

    // 1. The returned contract, then the persisted one.
    let usage = contract
        .completion
        .as_ref()
        .and_then(|c| c.usage)
        .unwrap_or_else(|| panic!("a codex child's contract carries its usage: {contract:#?}"));
    let responses = usage.output / USAGE.output;
    assert!(responses >= 1, "at least one canned response: {usage:?}");
    assert_eq!(
        usage,
        canned_times(responses),
        "every counter in its own field, {responses} canned responses' worth"
    );
    assert_eq!(
        persisted(&state, &task).completion.and_then(|c| c.usage),
        Some(usage),
        "the contract on disk carries what the returned one does"
    );

    // 2. One journal record for the node, with the same figure.
    let journal = env.project_dir.journal();
    let tree = read_path(&journal).expect("the journal reads");
    let node = tree
        .nodes()
        .iter()
        .find(|n| n.contracts.iter().any(|c| c.task_id == task))
        .expect("the child's node is in the journal");
    assert_eq!(
        usage_records(&journal, &node.agent_id),
        vec![usage],
        "exactly one record of the run's spend"
    );

    // 3. A fresh replay — what a restarted supervisor folds — carries it, no stream read.
    std::fs::remove_file(env.project_dir.agent(&node.agent_id).events())
        .expect("the node's stream existed");
    let restarted = read_path(&journal).expect("the journal reads again");
    let replayed = restarted.get(&node.agent_id).unwrap();
    assert_eq!(replayed.usage, Some(usage));
    // And the per-turn spend a sparkline draws, which adds up to the run.
    assert!(!replayed.turns.is_empty(), "codex's turns are recorded");
    assert_eq!(replayed.turns.iter().sum::<u64>(), usage.total());
}

fn persisted(state: &Path, task: &TaskId) -> TaskContract {
    let wanted = format!("{}.json", task.0);
    let found: Vec<_> = persisted_contracts(state)
        .unwrap_or_else(|e| panic!("{} does not enumerate: {e}", state.display()))
        .into_iter()
        .filter(|c| c.path.file_name().is_some_and(|f| f == wanted.as_str()))
        .collect();
    let [one] = found.as_slice() else {
        panic!("expected one persisted contract for {}", task.0);
    };
    serde_json::from_slice(&std::fs::read(&one.path).unwrap()).expect("the contract parses")
}

fn usage_records(journal: &Path, agent: &AgentId) -> Vec<TokenUsage> {
    std::fs::read(journal)
        .unwrap()
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(decode)
        .filter_map(|r| match r.kind {
            RecordKind::UsageRecorded(u) if &u.agent_id == agent => Some(u.usage),
            _ => None,
        })
        .collect()
}
