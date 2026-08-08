//! L1 property tests for §6.7's cap (design §8).
//!
//! These pin the guarantees the spec makes in prose, including the three that took several audit
//! rounds to get right: convergence, per-stream flags, and prefix-preserving path shortening.

use std::path::PathBuf;
use std::time::Duration as StdDuration;

use marion_core::cap::{BACKSTOP, MAX_COMMITS, MAX_EVIDENCE, PATH_CAP, cap_for_return};
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
            harness: marion_core::Harness::Codex,
            version: "0.146.0".into(),
            // `codex exec` carries no model argument, so a Codex contract names none.
            model: None,
        },
        repo: RepoIdentity {
            git_common_dir: Some("/repo/.git".into()),
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
        result_commits_omitted: 0,
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
    // **The one list a foreign agent fills.** Every other field above is marion's own derivation,
    // bounded by what marion chose to record; `result_commits` is whatever the child sent. Rule 6's
    // terminal promises a size that does not depend on the input, and this is the input that can
    // break it — 5 000 object names are ~200 KB encoded, past the backstop on their own.
    comp.result_commits = (0..5_000).map(|i| Oid(format!("{i:040x}"))).collect();

    let out = cap_for_return(contract(comp));
    assert!(
        encoded(&out) <= BACKSTOP,
        "cap must converge; got {} bytes against a {BACKSTOP} backstop",
        encoded(&out)
    );
}

/// **Rule 6's terminal claim, asserted rather than commented.**
///
/// `stub`'s own comment reads *"field set is fixed and small, so its size does not depend on the
/// input"*. That was true only while `result_commits` was hardcoded empty in `build_contract`; the
/// moment a child's commits were threaded through, the last resort became O(n) in whatever a
/// foreign agent sent.
///
/// Reaching rule 6 takes deliberate construction — rules 5(a)–(e) converge on almost everything —
/// so the criteria here are sized to survive 5(e)'s own cap (32 entries × 2 KiB = 64 KiB, over the
/// 48 KiB backstop) and force the terminal. The commit list then has to be cleared *there*, not
/// merely truncated at 5(d), or the stub's size still depends on the child.
#[test]
fn the_terminal_stub_clears_the_child_s_commits_and_says_how_many() {
    let mut comp = completion();
    comp.result_commits = (0..10_000).map(|i| Oid(format!("{i:040x}"))).collect();
    let mut c = contract(comp);
    c.acceptance_criteria = (0..64).map(|_| Capped::whole("c".repeat(8192))).collect();

    let out = cap_for_return(c);
    let got = out.completion.as_ref().unwrap();
    assert!(
        got.narrative
            .as_ref()
            .is_some_and(|n| n.value.is_empty() && n.truncated),
        "the premise: this input must actually reach rule 6, or the assertions below are vacuous"
    );
    assert!(
        encoded(&out) <= BACKSTOP,
        "the terminal must not depend on the child's input; got {} bytes",
        encoded(&out)
    );
    assert!(
        got.result_commits.is_empty(),
        "rule 6 clears every list it counts, and this is the only one a child filled"
    );
    assert_eq!(
        got.result_commits_omitted, 10_000,
        "and it says how many it dropped, or a reader cannot tell an elided list from an empty one \
         — the distinction changed_paths_omitted exists for"
    );
}

/// The ordinary path: a list marion can afford to keep is kept whole, and one over the cap is
/// elided **with a count**, so a reader can tell a short list from a shortened one.
///
/// The second half needs the document to still be over the backstop when 5(d) runs, since that is
/// the rule that elides these — hence the oversized `instructions`, which only 5(e) can shrink and
/// which therefore keeps the document large through (a)–(d) without touching any list.
#[test]
fn a_commit_list_over_the_cap_is_elided_with_a_count_and_a_short_one_is_untouched() {
    let mut comp = completion();
    comp.result_commits = (0..MAX_COMMITS).map(|i| Oid(format!("{i:040x}"))).collect();
    let out = cap_for_return(contract(comp.clone()));
    let kept = out.completion.as_ref().unwrap();
    assert_eq!(
        kept.result_commits.len(),
        MAX_COMMITS,
        "exactly at the cap is not over it"
    );
    assert_eq!(kept.result_commits_omitted, 0);

    comp.result_commits = (0..MAX_COMMITS + 7)
        .map(|i| Oid(format!("{i:040x}")))
        .collect();
    let mut c = contract(comp);
    c.instructions = Capped::whole("i".repeat(120_000));
    let out = cap_for_return(c);
    let got = out.completion.as_ref().unwrap();
    assert_eq!(got.result_commits.len(), MAX_COMMITS);
    assert_eq!(
        got.result_commits_omitted, 7,
        "the count is the difference, so a reader can reconstruct how many the child claimed"
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
    assert_eq!(
        v["child"]["harness"], "codex",
        "the harness is its bare wire string, never the enum variant's name"
    );
    assert!(
        v["child"]["model"].is_null(),
        "a codex child names no model: `codex exec` carries no model argument"
    );
}

/// `child.model` is **additive**. A contract persisted before the field existed must still read
/// back, because §6.7's audit record outlives the code that wrote it and a state directory is not
/// migrated between runs. The absence deserializes to the same `None` a codex child writes today,
/// which is the honest reading: that contract never recorded a model either.
#[test]
fn a_contract_persisted_before_child_model_existed_still_deserializes() {
    let mut v: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&contract(completion())).unwrap()).unwrap();
    v["child"]
        .as_object_mut()
        .unwrap()
        .remove("model")
        .expect("the field is written");
    let back: TaskContract = serde_json::from_value(v).expect("an older contract still reads");
    assert_eq!(back.child.model, None);
    assert_eq!(back, contract(completion()));
}

/// And when a harness *did* carry one, it is a bare string beside the harness — not a nested
/// object, and not folded into `version`.
#[test]
fn a_recorded_model_is_a_bare_string_beside_the_harness() {
    let mut c = contract(completion());
    c.child.model = Some("gemini-2.5-flash".into());
    let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(v["child"]["model"], "gemini-2.5-flash");
    assert_eq!(v["child"]["version"], "0.146.0", "and version is untouched");
    let back: TaskContract = serde_json::from_value(v).unwrap();
    assert_eq!(back, c);
}
