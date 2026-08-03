//! L1 property tests for §6.7's cap (design §8).
//!
//! These pin the guarantees the spec makes in prose, including the three that took several audit
//! rounds to get right: convergence, per-stream flags, and prefix-preserving path shortening.

use std::path::PathBuf;
use std::time::Duration as StdDuration;

use marion_core::cap::{BACKSTOP, MAX_EVIDENCE, PATH_CAP, cap_for_return};
use marion_core::contract::*;
use marion_core::encoding::{Duration, Millis, SystemTime};

fn cmd() -> Command {
    Command {
        program: "sh".into(),
        args: vec!["-c".into(), "cargo test".into()],
        cwd: "/tmp/wt".into(),
        timeout: Duration::from_secs(300),
    }
}

fn outcome(out: &str, err: &str) -> CommandOutcome {
    CommandOutcome {
        command: cmd(),
        exit_code: Some(0),
        stdout: Capped::whole(out),
        stderr: Capped::whole(err),
        duration: Millis(StdDuration::from_millis(412)),
        timed_out: false,
    }
}

fn contract(comp: Completion) -> TaskContract {
    TaskContract {
        task_id: TaskId("019fbf94-53c8-7c60-9f4c-12695a5e79fe".into()),
        requester: AgentId("019fbf94-0000-7000-8000-000000000001".into()),
        child: ChildRef {
            harness: "codex".into(),
            version: "0.146.0".into(),
        },
        repo: RepoIdentity {
            git_common_dir: "/repo/.git".into(),
            head_branch: Some("main".into()),
        },
        base_commit: Oid("a".repeat(40)),
        workspace: Workspace::Worktree {
            path: "/tmp/wt".into(),
            branch: "marion/t1".into(),
        },
        instructions: Capped::whole("add a flag"),
        acceptance_criteria: vec![Capped::whole("tests pass")],
        allowed_tools: vec!["apply_patch".into()],
        scope_ceiling: vec![Glob("**".into())],
        scope_requested: vec![Glob("src/**".into())],
        timeout: Duration::from_secs(900),
        verification: vec![cmd()],
        timestamps: TaskTimestamps {
            spawned: SystemTime::from_unix_millis(1_785_625_628_619),
            first_output: None,
            reported: None,
            exited: None,
        },
        completion: Some(comp),
    }
}

fn completion() -> Completion {
    Completion {
        status: ExitStatus::Ok,
        died_before_gate: false,
        reported_early: false,
        held_to_timeout: false,
        live_descendants_at_report: vec![],
        narrative: Some(Capped::whole("did the thing")),
        narrative_synthesized: false,
        result_commits: vec![],
        changed_paths: vec!["src/main.rs".into()],
        acceptance_criteria_omitted: 0,
        changed_paths_omitted: 0,
        scope_violations_omitted: 0,
        scope_enforced: true,
        scope_violations: vec![],
        diff: Some(Capped::whole("+line\n")),
        evidence: vec![outcome("ok", "")],
        evidence_omitted: 0,
        exit: ProcessExit {
            code: Some(0),
            signal: None,
            description: "clean exit".into(),
        },
    }
}

fn encoded(c: &TaskContract) -> usize {
    serde_json::to_vec(c).unwrap().len()
}

#[test]
fn ordinary_contract_is_untouched_and_well_under_the_backstop() {
    let c = contract(completion());
    let out = cap_for_return(c.clone());
    assert_eq!(out, c, "a small contract must pass through unchanged");
    assert!(encoded(&out) < BACKSTOP / 4);
}

#[test]
fn converges_on_pathological_input() {
    // Control characters escape to six bytes each in JSON, so raw-byte budgets alone cannot bound
    // the encoded size. This is the case rules 5-6 exist for.
    let mut comp = completion();
    comp.narrative = Some(Capped::whole("\u{1}".repeat(200_000)));
    comp.diff = Some(Capped::whole("\u{2}".repeat(200_000)));
    comp.evidence = (0..64)
        .map(|_| outcome(&"\u{3}".repeat(50_000), "\u{4}"))
        .collect();
    comp.changed_paths = (0..5_000)
        .map(|i| PathBuf::from(format!("src/f{i}.rs")))
        .collect();
    comp.scope_violations = comp.changed_paths.clone();

    let out = cap_for_return(contract(comp));
    assert!(
        encoded(&out) <= BACKSTOP,
        "cap must converge; got {} bytes against a {BACKSTOP} backstop",
        encoded(&out)
    );
}

#[test]
fn a_cap_can_never_hide_a_scope_violation() {
    let mut comp = completion();
    comp.scope_violations = (0..5_000)
        .map(|i| PathBuf::from(format!("outside/f{i}.rs")))
        .collect();
    comp.narrative = Some(Capped::whole("x".repeat(300_000)));
    let out = cap_for_return(contract(comp));
    let c = out.completion.unwrap();
    // Either every violation survived, or the counter records exactly what was dropped — the fact
    // of the violation always survives even when the path list does not.
    assert!(
        !c.scope_violations.is_empty() || c.scope_violations_omitted > 0,
        "violations vanished with no counter"
    );
    assert_eq!(c.scope_violations.len() + c.scope_violations_omitted, 5_000);
}

#[test]
fn streams_carry_independent_truncation_flags() {
    let mut comp = completion();
    comp.evidence = vec![outcome(&"o".repeat(100_000), "short")];
    let out = cap_for_return(contract(comp));
    let ev = &out.completion.unwrap().evidence[0];
    assert!(ev.stdout.truncated, "stdout was cut and must say so");
    assert!(
        !ev.stderr.truncated,
        "stderr fitted; a shared flag would have lied about it"
    );
    assert_eq!(ev.stderr.value, "short");
}

#[test]
fn diff_keeps_its_head_and_streams_keep_their_tail() {
    let mut comp = completion();
    comp.diff = Some(Capped::whole(format!("HEAD{}", "d".repeat(100_000))));
    comp.evidence = vec![outcome(&format!("{}TAIL", "o".repeat(100_000)), "")];
    let out = cap_for_return(contract(comp)).completion.unwrap();
    assert!(
        out.diff.unwrap().value.starts_with("HEAD"),
        "a diff only parses from the start"
    );
    assert!(
        out.evidence[0].stdout.value.ends_with("TAIL"),
        "a summary lands at the end"
    );
}

#[test]
fn shortened_paths_keep_the_prefix_that_proves_scope() {
    let long = format!("outside/{}/deep/file.rs", "seg/".repeat(200));
    assert!(long.len() > PATH_CAP);
    let mut comp = completion();
    // Enough long paths that the document is still over the backstop after (a)-(c), so rule 5(d)
    // actually runs. Path shortening is backstop-only by design.
    comp.scope_violations = (0..300).map(|_| PathBuf::from(&long)).collect();
    let out = cap_for_return(contract(comp)).completion.unwrap();
    assert!(out.scope_violations_omitted > 0 || !out.scope_violations.is_empty());
    if let Some(p) = out.scope_violations.first() {
        let s = p.to_string_lossy();
        assert!(
            s.starts_with("outside/"),
            "the prefix is what shows a path out of scope; trailing-only would erase it"
        );
        assert!(
            s.len() <= PATH_CAP,
            "a cut must never lengthen: {} bytes",
            s.len()
        );
    }
}

#[test]
fn evidence_collection_cap_records_what_it_dropped() {
    let mut comp = completion();
    comp.evidence = (0..40).map(|_| outcome("ok", "")).collect();
    let out = cap_for_return(contract(comp)).completion.unwrap();
    assert_eq!(out.evidence.len(), MAX_EVIDENCE);
    assert_eq!(out.evidence.len() + out.evidence_omitted, 40);
}

#[test]
fn contract_round_trips_through_serde() {
    let c = contract(completion());
    let s = serde_json::to_string(&c).unwrap();
    let back: TaskContract = serde_json::from_str(&s).unwrap();
    assert_eq!(back, c);
}

#[test]
fn pinned_json_encodings_hold() {
    let c = contract(completion());
    let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert!(
        v["child"].is_object(),
        "child is a named object, not a two-element array"
    );
    assert!(
        v["workspace"]["Worktree"].is_object(),
        "Workspace is externally tagged"
    );
    assert_eq!(v["completion"]["status"], "Ok");
    assert!(
        v["completion"]["diff"].is_object(),
        "diff is Capped, not a bare string"
    );
    assert_eq!(v["timeout"], 900, "a bound is integer seconds");
    assert_eq!(v["timestamps"]["spawned"], "2026-08-01T23:07:08.619Z");
}
