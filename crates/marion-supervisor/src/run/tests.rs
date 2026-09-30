//! Tests for `run.rs`, moved out of it unchanged.

use super::*;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGKILL: i32 = 9;
use crate::spawn::ChildOutcome;
use marion_core::agent_type::builtin;
use marion_core::harness::Harness;
use marion_harness::adapter_for;
use marion_testsupport::{Scratch, scratch};
use std::sync::Mutex;

/// **A reviewer's contract names its row's read-only switch and reads its findings**; on a row
/// that cannot refuse a write (scope-only, or an unverified tools axis) it also says so in
/// plain words, derived from the row's strategy rather than the harness's name.
#[test]
fn a_reviewer_contract_says_when_its_harness_cannot_be_made_read_only() {
    use marion_harness::spec::ReadOnly;
    let contract = || {
        crate::spawn::build_contract(
            TaskId("t".into()),
            AgentId("child".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: None,
                head_branch: None,
            },
            None,
            Workspace::SharedCwd { path: "/r".into() },
            "review",
            &[],
            &[Glob("**".into())],
            &[],
            Duration::from_secs(60),
            SystemTime(std::time::SystemTime::now()),
            &ChildOutcome {
                narrative: Some(
                    r#"{"verdict":"allow","findings":[{"severity":"low","file":"a.rs","claim":"x"}]}"#
                        .into(),
                ),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            Some(vec![]),
            None,
            vec![],
            vec![],
        )
    };
    let target = crate::review::Target {
        agent_id: AgentId("child".into()),
        commit: None,
        changed_paths: vec!["a.rs".into()],
    };
    for (ro, warned) in [
        (
            ReadOnly::ToolsAxis {
                verified: true,
                note: "m",
            },
            false,
        ),
        (
            ReadOnly::Pair {
                key: "k",
                value: "v",
                note: "m",
            },
            false,
        ),
        (
            ReadOnly::EnvVar {
                key: "k",
                value: "v",
                note: "m",
            },
            false,
        ),
        (
            ReadOnly::ToolsAxis {
                verified: false,
                note: "m",
            },
            true,
        ),
        (ReadOnly::ScopeOnly { note: "m" }, true),
    ] {
        let mut c = contract();
        let tally = record_review(&mut c, &target, ro).expect("a readable report is tallied");
        assert_eq!(tally.findings, 1, "{ro:?}");
        assert!(
            c.allowed_tools
                .contains(&format!("read-only:{}", ro.kind()))
        );
        let comp = c.completion.unwrap();
        assert_eq!(comp.findings.map(|f| f.findings.len()), Some(1));
        assert_eq!(
            comp.exit.description.contains(crate::review::UNGUARDED),
            warned,
            "{ro:?}: {}",
            comp.exit.description
        );
    }
}

/// **The cap is on the lines as the journal encodes them, at its exact boundary.** `["…"]`
/// costs four bytes of framing around one line, so a line of `cap - 4` bytes is exactly the
/// cap and is served, and one byte more is refused naming both numbers.
#[test]
fn verification_is_capped_at_its_encoded_size_on_the_boundary() {
    let line = |n: usize| vec!["a".repeat(n)];
    assert!(check_verification_size(&[]).is_ok(), "no lines, no cost");
    assert!(check_verification_size(&line(MAX_VERIFICATION_BYTES - 4)).is_ok());
    match check_verification_size(&line(MAX_VERIFICATION_BYTES - 3)) {
        Err(SpawnError::VerificationTooLarge { bytes, cap }) => {
            assert_eq!(
                (bytes, cap),
                (MAX_VERIFICATION_BYTES + 1, MAX_VERIFICATION_BYTES)
            );
        }
        other => panic!("one byte over is refused by name, got {other:?}"),
    }
}

/// **Escaping is counted, because the record carries the escaped bytes.** A NUL encodes as
/// six (`\u0000`), so 2000 of them are well under the cap raw and far over it on disk — and a
/// raw-length guard would let through an intent the journal then drops whole.
#[test]
fn verification_escaping_and_many_lines_count_against_the_cap() {
    match check_verification_size(&["\0".repeat(2000)]) {
        Err(SpawnError::VerificationTooLarge { bytes, .. }) => {
            assert_eq!(bytes, 2000 * 6 + 4)
        }
        other => panic!("the encoded size is what is capped, got {other:?}"),
    }
    let many = vec!["x".to_string(); MAX_VERIFICATION_BYTES / 4];
    assert!(
        matches!(
            check_verification_size(&many),
            Err(SpawnError::VerificationTooLarge { .. })
        ),
        "the cap is on the total, not per line"
    );
}

/// **Whose work a new worktree carries is decided by where the caller's tree is.** A caller in
/// a worktree marion made under this project hands its child its uncommitted work; a caller in
/// the operator's own checkout does not, and that checkout is left exactly as it was.
#[test]
fn only_a_caller_in_a_marion_worktree_hands_its_child_its_uncommitted_work() {
    let dir = scratch("select-workspace-carry");
    let repo = fixture_repo(&dir);
    let project = ProjectDir::new(&dir.join("state"), &repo);
    let agent_type = builtin("codex").unwrap();
    let request = |from: &Path| SpawnRequest {
        review: None,
        race: None,
        agent_type: "codex".into(),
        prompt: "test it".into(),
        repo: from.to_path_buf(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec![],
        timeout_secs: 1,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        budget: None,
        read_only: false,
        workflow: None,
    };
    let caller_wt = project.agent(&AgentId("caller".into())).worktree();
    std::fs::create_dir_all(caller_wt.parent().unwrap()).unwrap();
    crate::spawn::make_worktree(
        crate::spawn::Tree::Operator(&repo),
        &caller_wt,
        &["marion/caller".into()],
        false,
    )
    .unwrap();
    std::fs::write(caller_wt.join("src/keep.txt"), "the caller's edit\n").unwrap();
    std::fs::write(repo.join("src/keep.txt"), "the operator's edit\n").unwrap();

    let child = AgentId("child".into());
    // Held: a `PrelaunchWorktree` dropped here would take the new tree back at once.
    let (ws, _, _, _child_tree) = select_workspace(
        &request(&caller_wt),
        &agent_type,
        &project,
        &repo,
        &TaskId("t-child".into()),
        &child,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.path().join("src/keep.txt")).unwrap(),
        "the caller's edit\n"
    );

    let sibling = AgentId("sibling".into());
    let (ws, _, _, _sibling_tree) = select_workspace(
        &request(&repo),
        &agent_type,
        &project,
        &repo,
        &TaskId("t-sibling".into()),
        &sibling,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.path().join("src/keep.txt")).unwrap(),
        "keep\n",
        "a child of the operator's checkout starts from its HEAD"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/keep.txt")).unwrap(),
        "the operator's edit\n"
    );
    let log = SysCommand::new("git")
        .current_dir(&repo)
        .args(["rev-list", "--count", "main"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "1",
        "and no commit was made on the operator's branch"
    );
}

/// **A resume takes the tree its session was created in, and cuts none.**
///
/// The two arms of [`select_workspace`] are wrong for a second life in opposite ways:
/// `Worktree` would run `make_worktree` on a tree that already exists (which fails) or produce
/// a *fresh* tree the resumed session has never seen, and `SharedCwd` would read `req.repo`
/// rather than where the node actually ran. So the recorded workspace short-circuits both.
///
/// The oracle is that `req` still says `Worktree` and `req.repo` is **not a repository at
/// all**: without the resume this call is `NotAGitRepo`, so returning the recorded tree proves
/// the recorded value was read and neither arm ran.
#[test]
fn a_resume_takes_the_recorded_workspace_instead_of_cutting_a_second_one() {
    let dir = scratch("select-workspace-resume");
    let repo = dir.join("not-a-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let first_life = dir.join("first-life-worktree");
    std::fs::create_dir_all(&first_life).unwrap();
    let recorded = Workspace::Worktree {
        path: first_life.clone(),
        branch: "marion/t-1".into(),
    };
    let agent_type = builtin("claude").unwrap();
    let agent_id = AgentId("child".into());
    let project = ProjectDir::new(&dir.join("state"), &repo);
    let task_id = TaskId("t-1".into());
    let mut req = SpawnRequest {
        budget: None,
        review: None,
        race: None,
        agent_type: "claude".into(),
        prompt: "carry on".into(),
        repo: repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec![],
        timeout_secs: 1,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        read_only: false,
        workflow: None,
    };
    assert!(
        matches!(
            select_workspace(&req, &agent_type, &project, &repo, &task_id, &agent_id),
            Err(SpawnError::NotAGitRepo { .. })
        ),
        "the fixture's repo is deliberately not a repository, so a fresh spawn cannot cut a \
         worktree in it — which is what makes the resume below unambiguous"
    );

    req.resume = Some(ChildResume {
        agent_id: agent_id.clone(),
        session: "sess-1".into(),
        workspace: recorded.clone(),
        usage: None,
    });
    let (workspace, _base, _claim, _prelaunch) =
        select_workspace(&req, &agent_type, &project, &repo, &task_id, &agent_id)
            .expect("the recorded tree needs no repository question asked of it");
    assert_eq!(
        workspace, recorded,
        "the relaunch runs where the journal says the session was created"
    );
    assert!(
        !project.agent(&agent_id).worktree().exists(),
        "and no second tree was cut under the agent dir"
    );

    // **Gone by the time the launch reaches it**: removed after `node/resume` looked, so the
    // launch refuses it by name rather than starting a process in a directory that is not there.
    std::fs::remove_dir_all(&first_life).unwrap();
    let gone =
        select_workspace(&req, &agent_type, &project, &repo, &task_id, &agent_id).map(|(w, ..)| w);
    assert!(
        matches!(&gone, Err(SpawnError::ResumeTreeGone { path }) if path == &first_life),
        "a resume into a removed tree is refused at the launch too: {gone:?}"
    );
}

/// **The journal's terminal record is never durable before the contract it is about.**
///
/// `0827fc8` made the *stream's* closing bookend mean *"everything about this node is on
/// disk"* by writing the contract before it, and said in as many words that the ordering is
/// load-bearing. The **journal's** terminal record — `RecordKind::Exited`, which is what
/// `Replay` folds into `NodeState::Exited`, and so what every reader of the journal takes to
/// mean the node is over — was still written first, leaving exactly the same window one
/// `write(2)` wide on the other surface. It was not hypothetical: `depth_gate.rs` measured it
/// on gemini as a run with zero contracts where one was about to exist, and worked around it by
/// polling for both conditions instead of asserting the rule.
///
/// **Observed from inside the window, not sampled from outside it.** The difference between the
/// right order and the wrong one is purely temporal — both orders end with the same records and
/// the same file — so there is no after-the-fact reading that can tell them apart. A poll from
/// another thread would have to catch one `create` plus one `write`, and would pass against the
/// broken order almost every time; a test that usually passes against the defect is worse than
/// none. So the seam offers the one instant that matters ([`at_contract_write`]) and this reads
/// the journal from within it.
#[test]
fn the_terminal_record_is_not_journaled_until_the_contract_is_on_disk() {
    use marion_core::contract::*;
    use marion_core::encoding::{Duration as EncDuration, SystemTime as EncSystemTime};

    let dir = scratch("run-exit-order");
    let project = marion_core::paths::ProjectDir::new(&dir, std::path::Path::new("/repo/.git"));
    std::fs::create_dir_all(project.path()).expect("the project dir");
    let agent_id = AgentId("019fbf94-0000-7000-8000-0000000000aa".into());
    let agent_dir = project.agent(&agent_id);

    let contract = crate::spawn::build_contract(
        TaskId("019fbf94-53c8-7c60-9f4c-12695a5e79fe".into()),
        AgentId("019fbf94-0000-7000-8000-000000000001".into()),
        marion_core::Harness::Codex,
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
        EncDuration::from_secs(900),
        EncSystemTime::from_unix_millis(1_785_625_628_619),
        &ChildOutcome {
            narrative: Some("done".into()),
            exit_code: Some(0),
            ..Default::default()
        },
        Some(vec![]),
        None,
        vec![],
        vec![],
    );
    assert!(
        contract.completion.is_some(),
        "the premise: only a contract with a completion produces an `Exited` record at all, so \
         a fixture without one would make every assertion below vacuous"
    );

    // Read from inside the window. `terminal` is what the journal said at the instant the
    // contract file was about to be created.
    let terminal_at_write = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let journal_path = project.journal();
    let seen = std::sync::Arc::clone(&terminal_at_write);
    let watched = agent_id.clone();
    let _hook = at_contract_write::install(move || {
        let bytes = std::fs::read(&journal_path).unwrap_or_default();
        let exited = marion_core::registry::replay(&bytes)
            .nodes()
            .iter()
            .any(|n| n.agent_id == watched && n.state.is_exited());
        seen.store(exited, std::sync::atomic::Ordering::SeqCst);
    });

    persist_contract_then_record_exit(&project, &agent_dir, &agent_id, &contract, false)
        .expect("the contract is written to a directory this test owns");

    assert!(
        !terminal_at_write.load(std::sync::atomic::Ordering::SeqCst),
        "**the rule.** At the instant the contract was about to be created the journal already \
         said this node had exited, so a reader acting on the terminal record — which is what a \
         terminal record is for — would have found no contract, and could not tell `not written \
         yet` from `never written` (§4.1)"
    );

    // And both halves really happened, so the assertion above is not satisfied by a run that
    // did nothing.
    assert!(
        agent_dir.contract(&contract.task_id).is_file(),
        "the contract reached disk"
    );
    let bytes = std::fs::read(project.journal()).expect("the journal was written");
    assert!(
        marion_core::registry::replay(&bytes)
            .nodes()
            .iter()
            .any(|n| n.agent_id == agent_id && n.state.is_exited()),
        "and the terminal record followed it"
    );
}

/// **`run_bounded` survives a duration no clock can hold, having already started a process.**
///
/// The order is what makes this a leak rather than an error: `Command::spawn` runs first, the
/// deadline is computed second, and `Instant + Duration` panics on overflow. Dropping a
/// `std::process::Child` kills nothing, so the unwind abandoned a live process — and on the
/// bridge's own thread it took the whole MCP server down with it.
///
/// Asserted here rather than only through `run_spawn` because the clamp
/// ([`effective_timeout`]) lives in `run_spawn`, and a `pub` function whose liveness depends on
/// a guard one of its callers happens to apply is a defect waiting for the second caller.
/// `run_bounded` already has one that is not `run_spawn`: the end-to-end test bounds a real
/// `marion run` with it.
#[test]
fn a_duration_the_clock_cannot_represent_bounds_the_run_rather_than_unwinding_it() {
    let out = run_bounded(SysCommand::new("true").arg("--"), StdDuration::MAX)
        .expect("an unrepresentable bound is still a bound, not a panic");
    assert!(
        !out.timed_out,
        "the fallback deadline is the cap, not `now` — saturating to now would kill a healthy \
         child instantly, which is the most aggressive possible reading of `too long`"
    );
    assert_eq!(
        out.code,
        Some(0),
        "and the process really ran, and was really reaped"
    );
}

/// **Every spawn-time probe carries the row's no-self-update switch**, for each shape of
/// switch a shipped row takes: an Env row (claude) and a Pair row (codex, on its own `-c`). No
/// shipped row takes a Document switch since the gemini CLI row was retired; `probe`'s own test
/// holds that shape on a synthetic row. `harness_version` is the probe `run_spawn`,
/// `root::launch_inner` and the native launch all take.
#[test]
fn the_spawn_probe_carries_every_shape_of_switch() {
    for h in [Harness::ClaudeCode, Harness::Codex] {
        let dir = scratch(&format!("vprobe-switch-{h}"));
        let (watch, want) = version_fake::switch_evidence(h);
        let watch: Vec<&str> = watch.iter().map(String::as_str).collect();
        let fake = version_fake::write(&dir, "harness", "9.9.9 (fake)", &watch);
        assert_eq!(
            harness_version(fake.to_str().unwrap(), h),
            "9.9.9 (fake)",
            "{h}"
        );
        let probes = version_fake::probes(&dir);
        assert_eq!(probes.len(), 1, "{h}: {probes:?}");
        for line in &want {
            assert!(
                probes[0].contains(line),
                "{h}: no {line:?} in {:?}",
                probes[0]
            );
        }
    }
}

/// A row whose binary may update itself and has no known switch is **not run** to read a
/// version: the probe is refused and the version is `"unknown"`.
#[test]
fn a_row_with_no_known_switch_is_never_probed() {
    let dir = scratch("vprobe-refused");
    let fake = version_fake::write(&dir, "agent", "1.0.0", &[]);
    let refused: Vec<Harness> = Harness::ALL
        .into_iter()
        .filter(|h| {
            matches!(
                marion_harness::adapter::harness_spec(*h).updates,
                marion_harness::spec::UpdatePolicy::None { .. }
            )
        })
        .collect();
    assert!(!refused.is_empty(), "the sweep needs a row to refuse");
    for h in refused {
        assert_eq!(harness_version(fake.to_str().unwrap(), h), "unknown", "{h}");
    }
    assert!(
        version_fake::probes(&dir).is_empty(),
        "the binary never ran"
    );
}

/// **One probe per binary, until the binary changes.** Two spawns of the same unchanged file
/// fork `--version` once; touching it (mtime) or rewriting it (inode, ctime) probes again.
#[test]
fn the_version_is_read_once_per_binary_and_again_when_it_changes() {
    let dir = scratch("vprobe-cache");
    let fake = version_fake::write(&dir, "claude", "1.0.0", &[]);
    let program = fake.to_str().unwrap();
    let h = Harness::ClaudeCode;
    assert_eq!(harness_version(program, h), "1.0.0");
    assert_eq!(harness_version(program, h), "1.0.0");
    assert_eq!(
        version_fake::probes(&dir).len(),
        1,
        "the second spawn used the cache"
    );

    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&fake)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(harness_version(program, h), "1.0.0");
    assert_eq!(
        version_fake::probes(&dir).len(),
        2,
        "a touched binary is read again"
    );

    let tmp = version_fake::write(&dir, "claude.new", "2.0.0", &[]);
    std::fs::rename(&tmp, &fake).unwrap();
    assert_eq!(
        harness_version(program, h),
        "2.0.0",
        "a replaced binary's new version"
    );
    assert_eq!(harness_version(program, h), "2.0.0");
    assert_eq!(
        version_fake::probes(&dir).len(),
        3,
        "read once after the replacement"
    );

    let other = Harness::Codex;
    assert_eq!(harness_version(program, other), "2.0.0");
    assert_eq!(
        version_fake::probes(&dir).len(),
        4,
        "the same file probed as another harness runs that row's probe"
    );
}

/// A probe that fails is not cached: the next spawn asks again.
#[test]
fn a_failed_probe_is_asked_again() {
    let dir = scratch("vprobe-fail");
    let fake = dir.join("claude");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho probe >> \"$(dirname \"$0\")/probes.log\"\nexit 3\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let program = fake.to_str().unwrap();
    assert_eq!(harness_version(program, Harness::ClaudeCode), "unknown");
    assert_eq!(harness_version(program, Harness::ClaudeCode), "unknown");
    assert_eq!(version_fake::probes(&dir).len(), 2);
}

#[test]
fn a_harness_version_is_its_first_non_blank_line() {
    assert_eq!(
        version_line("GitHub Copilot CLI 1.0.83.\nRun 'copilot update' to check for updates.\n")
            .as_deref(),
        Some("GitHub Copilot CLI 1.0.83.")
    );
    assert_eq!(
        version_line("2.1.223 (Claude Code)\n").as_deref(),
        Some("2.1.223 (Claude Code)")
    );
    assert_eq!(version_line("\n  0.147.0  \n").as_deref(), Some("0.147.0"));
    assert_eq!(version_line(""), None, "nothing printed is not a version");
    assert_eq!(version_line("\n\n"), None);
}

/// **A signalled exit alone does not skip verification.** Every ACP child ends on marion's own
/// SIGINT (`acp_child::finish`), so `signal: Some(2), timed_out: false` is the *normal* end of
/// a finished ACP turn; the commands must run. Only marion's own cut — `timed_out` — skips.
#[test]
fn verification_runs_for_a_signalled_exit_and_skips_only_marions_own_timeout() {
    let dir = scratch("supervisor-verify-signalled");
    let cmds = verification_commands(&["echo ok".into()], &dir, None);
    let shut_down_by_marion = ChildOutcome {
        signal: Some(2),
        timed_out: false,
        ..ChildOutcome::default()
    };
    let evidence = verification_evidence(&shut_down_by_marion, &cmds);
    assert_eq!(
        evidence.len(),
        1,
        "a signalled, un-timed-out child is verified"
    );
    assert_eq!(evidence[0].exit_code, Some(0));
    assert_eq!(evidence[0].stdout.value, "ok\n");

    let cut_short = ChildOutcome {
        signal: Some(9),
        timed_out: true,
        ..ChildOutcome::default()
    };
    assert!(
        verification_evidence(&cut_short, &cmds).is_empty(),
        "marion's own timeout kill is the one exit that skips"
    );
}

/// **A node starts under the operator's ceilings** ([`crate::node_limits`]), read as the node's
/// own hard limit on open files.
#[test]
fn a_node_starts_under_the_open_file_ceiling() {
    let out = run_bounded(
        SysCommand::new("sh").args(["-c", "ulimit -Hn"]),
        StdDuration::from_secs(30),
    )
    .unwrap();
    let ceiling = crate::user_config::node_limits()
        .unwrap_or_default()
        .open_files;
    let inherited = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum;
    let expected = inherited.map_or(ceiling, |m| m.min(ceiling));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        expected.to_string()
    );
}

#[test]
fn an_overrunning_process_group_is_killed_and_reported_as_timed_out() {
    let dir = scratch("supervisor-timeout");
    let marker = dir.join("survived");
    let script = format!("(sleep 1; touch '{}') & sleep 10", marker.display());
    let out = run_bounded(
        SysCommand::new("sh").args(["-c", &script]),
        StdDuration::from_millis(50),
    )
    .unwrap();
    assert!(out.timed_out);
    assert_eq!(out.signal, Some(9));
    let contract = build_contract(
        TaskId("task".into()),
        AgentId("root".into()),
        marion_core::Harness::Codex,
        RepoIdentity {
            git_common_dir: Some("/r/.git".into()),
            head_branch: None,
        },
        Some(Oid("a".repeat(40))),
        Workspace::Worktree {
            path: "/wt".into(),
            branch: "b".into(),
        },
        "",
        &[],
        &[Glob("**".into())],
        &[Glob("**".into())],
        Duration::from_secs(1),
        SystemTime(std::time::SystemTime::now()),
        &ChildOutcome {
            timed_out: out.timed_out,
            signal: out.signal,
            ..ChildOutcome::default()
        },
        Some(vec![]),
        None,
        vec![],
        vec![],
    );
    assert_eq!(contract.completion.unwrap().status, ExitStatus::TimedOut);
    thread::sleep(StdDuration::from_millis(1100));
    assert!(
        !marker.exists(),
        "a descendant survived the killed process group"
    );
}

/// **A child that leaves its run early still records what it spent** — the spend the s2 claude
/// child lost at its timeout: an early return drops the guard, the guard writes the stream's
/// figure to the journal, and the contract path's own record is the only one when it runs.
#[test]
fn a_run_that_leaves_early_still_journals_what_its_stream_said_it_spent() {
    let dir = scratch("spend-on-every-end");
    let project = marion_core::paths::ProjectDir::new(&dir.join("state"), &dir.join("repo"));
    std::fs::create_dir_all(project.path()).unwrap();
    let id = AgentId("n-spend".into());
    let sink = crate::events::EventSink::new(
        crate::events::EventWriter::open_path(&dir.join("events.jsonl"), &id).unwrap(),
        Harness::ClaudeCode,
        "unused".into(),
    );
    // A turn in flight: one request's counters and no `result`, as a killed child leaves it.
    sink.record_line(
        r#"{"type":"assistant","message":{"id":"msg_1","usage":{"input_tokens":12,"output_tokens":3}}}"#,
    );
    let usage_records = || {
        std::fs::read_to_string(project.journal())
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("UsageRecorded"))
            .count()
    };
    let leave_early = || -> Result<(), SpawnError> {
        let mut spend = SpendOnEveryEnd {
            project: &project,
            agent_id: &id,
            events: Some(&sink),
            recorded: false,
            aborted: None,
            closes: true,
        };
        spend.aborting(Err(SpawnError::NodeAborted("the driver failed".into())))
    };
    assert!(leave_early().is_err());
    assert_eq!(usage_records(), 1, "the early return journaled the spend");
    // And closed the stream a blocking parent reads to its end, in the error's own words.
    let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    assert!(
        events.contains("Aborted") && events.contains("the driver failed"),
        "the early return wrote the closing bookend: {events}"
    );

    let mut spend = SpendOnEveryEnd {
        project: &project,
        agent_id: &id,
        events: Some(&sink),
        recorded: false,
        aborted: None,
        closes: true,
    };
    assert_eq!(spend.record().map(|u| (u.input, u.output)), Some((12, 3)));
    drop(spend);
    assert_eq!(
        usage_records(),
        2,
        "recorded once on the contract path, not again on drop"
    );
}

/// **A budget is refused on a harness whose stream reports no spend**, and allowed on one that
/// does; no budget is never refused.
#[test]
fn a_budget_is_refused_where_marion_cannot_count_the_spend() {
    let budget = marion_core::budget::Budget {
        tree_tokens: Some(1_000),
        ..Default::default()
    };
    let uncounted = marion_core::harness::Harness::ALL
        .into_iter()
        .find(|h| {
            marion_harness::adapter_for(*h)
                .ok()
                .is_some_and(|a| a.usage_rule().is_none())
        })
        .expect("some row states no usage rule");
    assert!(matches!(
        check_budget_enforceable(uncounted, Some(&budget)),
        Err(SpawnError::BudgetUnenforceable { .. })
    ));
    assert!(check_budget_enforceable(uncounted, None).is_ok());
    assert!(
        check_budget_enforceable(marion_core::harness::Harness::ClaudeCode, Some(&budget)).is_ok()
    );
}

/// A `LaunchOnly` child that writes `frame` every 100 ms and never ends on its own, run under a
/// 30 s bound through the codex row's reader — its frames app-server's `error` notification
/// (the conformance P-errors capture), whose error rules are what these cells exercise.
fn retrying_child(tag: &str, frame: &str) -> (ChildRun, StdDuration) {
    let dir = scratch(tag);
    let project = marion_core::paths::ProjectDir::new(&dir.join("state"), &dir.join("repo"));
    std::fs::create_dir_all(project.path()).unwrap();
    let id = AgentId(format!("n-{tag}"));
    let inv = Invocation {
        sandbox: None,
        inherit: None,
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!("while :; do echo '{frame}'; sleep 0.1; done"),
        ],
        env: vec![],
        cwd: dir.to_path_buf(),
        model: None,
        session_mode: None,
        env_remove: vec![],
    };
    let watch = crate::session_watch::SessionWatch::new(&project, &id, Harness::Codex, false);
    let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
    let at = Instant::now();
    let run = launch_only_child(
        &inv,
        &dir,
        StdDuration::from_secs(30),
        &|_| {},
        &watch,
        None,
        adapter.as_ref(),
    )
    .unwrap();
    (run, at.elapsed())
}

/// **A refused credential ends the run at once, whatever the harness's retry schedule** — no
/// retry heals a 401, so waiting the harness out spends the node's clock for nothing. The run
/// ends failed in the harness's own words, not timed out, and its cause is still auth.
#[test]
fn an_auth_refusal_the_harness_retries_ends_the_run_at_once() {
    let (run, took) = retrying_child(
        "run-auth-stop",
        r#"{"method":"error","params":{"error":{"message":"Reconnecting... 1/5","additionalDetails":"unexpected status 401 Unauthorized: Incorrect API key provided"},"willRetry":true}}"#,
    );
    assert!(took < StdDuration::from_secs(10), "took {took:?}");
    assert!(!run.exit.timed_out);
    let why = run.stopped.as_deref().expect("marion ended it");
    assert!(why.contains("401"), "{why}");
    let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
    assert!(matches!(
        attempt_cause(adapter.as_ref(), &run, None, None),
        Some(marion_core::contract::FailureCause::Auth { .. })
    ));
}

/// **A rate limit the harness retries is left to its backoff** — it can recover inside it, as
/// an outage can — and a run the bound then ends still records the cause the retries said.
#[test]
fn a_retried_rate_limit_is_waited_out_and_its_cause_survives_the_timeout() {
    let dir = scratch("run-limit-bound");
    let project = marion_core::paths::ProjectDir::new(&dir.join("state"), &dir.join("repo"));
    std::fs::create_dir_all(project.path()).unwrap();
    let id = AgentId("n-limit".into());
    let inv = Invocation {
        sandbox: None,
        inherit: None,
        program: "sh".into(),
        args: vec![
            "-c".into(),
            r#"while :; do echo '{"method":"error","params":{"error":{"message":"Reconnecting... 1/5","additionalDetails":"exceeded retry limit, last status: 429 Too Many Requests"},"willRetry":true}}'; sleep 0.1; done"#.into(),
        ],
        env: vec![],
        cwd: dir.to_path_buf(),
        model: None,
        session_mode: None,
        env_remove: vec![],
    };
    let watch = crate::session_watch::SessionWatch::new(&project, &id, Harness::Codex, false);
    let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
    let run = launch_only_child(
        &inv,
        &dir,
        StdDuration::from_secs(2),
        &|_| {},
        &watch,
        None,
        adapter.as_ref(),
    )
    .unwrap();
    assert!(run.exit.timed_out, "left to the harness until the bound");
    assert_eq!(run.stopped, None);
    // On the operator's own login a 429 is the account's window: the cause the contract
    // carries, a notice, and never a failover (`next_attempt`).
    assert!(matches!(
        attempt_cause(adapter.as_ref(), &run, None, None),
        Some(marion_core::contract::FailureCause::UsageLimit { .. })
    ));
}

/// **On the operator's own login only a refused credential fails over; a limit never switches
/// profiles.** Two profiles are listed; a run whose retries said 429 is the account's window —
/// its cause is recorded and nothing relaunches — while one that said 401 moves to the next
/// profile. The rule the coordinator pinned: a usage limit is a notice, never an account swap.
#[test]
fn a_limit_never_switches_profiles_and_a_refused_login_does() {
    let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
    let profile = |name: &str| crate::profiles::Profile {
        name: name.into(),
        harness: Harness::Codex,
        dir: format!("/profiles/{name}"),
    };
    let launch = crate::profiles::Launch {
        chain: vec![profile("work"), profile("personal")],
        paths: None,
    };
    let failed = |message: &str| ChildRun {
        stdout: format!(r#"{{"method":"error","params":{{"error":{{"message":"{message}"}}}}}}"#),
        stderr: String::new(),
        exit: ChildExit {
            code: Some(1),
            signal: None,
            timed_out: false,
        },
        capture_truncated: false,
        denied_permissions: vec![],
        stopped: None,
        last_turn_at: 0,
    };
    let wt = scratch("run-profile-policy");
    let next = |run: &ChildRun| {
        next_attempt(
            None,
            &launch,
            0,
            run,
            adapter.as_ref(),
            crate::spawn::Tree::Operator(&wt),
            None,
            StdDuration::from_secs(60),
        )
    };
    assert!(
        next(&failed(
            "exceeded retry limit, last status: 429 Too Many Requests"
        ))
        .is_none(),
        "a limit relaunches nothing"
    );
    assert!(matches!(
        attempt_cause(
            adapter.as_ref(),
            &failed("exceeded retry limit, last status: 429 Too Many Requests"),
            None,
            None
        ),
        Some(marion_core::contract::FailureCause::UsageLimit { .. })
    ));
    assert!(matches!(
        next(&failed(
            "unexpected status 401 Unauthorized: Incorrect API key provided"
        )),
        Some((
            Next::Profile(1),
            marion_core::contract::FailureCause::Auth { .. }
        ))
    ));
}

/// **An endpoint child's key never reaches its live event record.** `launch_only_child`
/// records each line as it lands, before the capture is redacted, so the node's sink has to
/// scrub the line; a harness echoing its key in an error is the case this defends.
#[test]
fn a_launch_only_childs_live_record_carries_no_endpoint_key() {
    let dir = scratch("run-live-redact");
    let project = marion_core::paths::ProjectDir::new(&dir.join("state"), &dir.join("repo"));
    std::fs::create_dir_all(project.path()).unwrap();
    let id = AgentId("n-redact".into());
    let events_path = dir.join("events.jsonl");
    let sink = crate::events::EventSink::new(
        crate::events::EventWriter::open_path(&events_path, &id).unwrap(),
        Harness::Codex,
        "unused".into(),
    )
    .scrubbing(Some("sk-endpoint-9f2c1e7a"));
    let key = "sk-endpoint-9f2c1e7a";
    let inv = Invocation {
        sandbox: None,
        inherit: None,
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(r#"echo '{{"type":"error","message":"bad key {key}"}}'"#),
        ],
        env: vec![],
        cwd: dir.to_path_buf(),
        model: None,
        session_mode: None,
        env_remove: vec![],
    };
    let watch = crate::session_watch::SessionWatch::new(&project, &id, Harness::Codex, false);
    let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
    let run = launch_only_child(
        &inv,
        &dir,
        StdDuration::from_secs(20),
        &|_| {},
        &watch,
        Some(&sink),
        adapter.as_ref(),
    )
    .unwrap();
    drop(sink);
    assert!(
        run.stdout.contains(key),
        "the capture is redacted by the caller, later"
    );
    let recorded = std::fs::read_to_string(&events_path).unwrap();
    assert!(recorded.contains("bad key ***"), "{recorded}");
    assert!(!recorded.contains(key), "{recorded}");
}

/// `kill(pid, 0)`: `ESRCH` is the only answer that means *gone*. `EPERM` means the process
/// exists and is not ours, which for this fix would still be a survivor.
fn alive(pid: i32) -> bool {
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    const ESRCH: i32 = 3;
    std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

#[test]
fn a_tool_call_child_that_setsid_escaped_the_group_is_dead_after_the_bound_expires() {
    // Case B from tests/fixtures/s7/README.md — the only leaking case: the escaped process is
    // STILL RUNNING when the bound expires. `POSIX::setsid()` reproduces what `codex exec` does
    // to every tool-call command: a new session AND a new process group, so `killpg` on the
    // group marion created cannot reach it.
    assert!(
        SysCommand::new("perl")
            .args(["-e", "1"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false),
        "this test needs perl to build a setsid escapee"
    );
    let dir = scratch("supervisor-setsid-escape");
    let pids = dir.join("pids");
    let script = format!(
        r#"use POSIX ();
               my $pid = fork();
               if ($pid == 0) {{
                   POSIX::setsid();
                   exec("/bin/sh", "-c", "sleep 900 & echo \$! >> '{p}'; wait");
               }}
               open(my $f, ">>", "{p}"); print $f "$pid\n"; close $f;
               sleep 900;"#,
        p = pids.display()
    );
    let out = run_bounded(
        SysCommand::new("perl").args(["-e", &script]),
        StdDuration::from_millis(1000),
    )
    .unwrap();
    assert!(out.timed_out);

    let recorded: Vec<i32> = std::fs::read_to_string(&pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    // Give a killed process a moment to leave the table; then clean up unconditionally, so a
    // failing assertion below can never leave a `sleep 900` behind — the very bug under test.
    let mut survivors: Vec<i32> = recorded.clone();
    for _ in 0..40 {
        survivors.retain(|p| alive(*p));
        if survivors.is_empty() {
            break;
        }
        thread::sleep(StdDuration::from_millis(50));
    }
    for p in &recorded {
        let _ = unsafe { kill(*p, SIGKILL) };
    }
    assert_eq!(
        recorded.len(),
        2,
        "expected the setsid'd shell and its sleep to record their pids, got {recorded:?}"
    );
    assert!(
        survivors.is_empty(),
        "processes in a session marion never created survived the timeout kill: {survivors:?}"
    );
}

/// The liveness property, on the path the kill sweep does *not* fix.
///
/// The escapee `setsid`s (a new session and a new process group, exactly what `codex exec` does
/// to every tool-call command) and keeps the inherited stdout open. `kill_tree` is injected as
/// a killer that reaches only the direct child — the standing-in-for-reality case where the
/// sweep misses something, e.g. the known race of a child forking into a fresh group between
/// the `ps` snapshot and the first signal. Before the drain bound this test did not fail, it
/// **hung**: `join` on a drain thread never returns while an escapee holds the write end.
///
/// The whole call runs on a worker thread behind a `recv_timeout`, so a regression fails loudly
/// instead of wedging CI, and the escapee is killed unconditionally before any assertion.
#[test]
fn the_bound_returns_even_when_an_escapee_survives_the_kill_and_holds_the_pipe() {
    assert!(
        SysCommand::new("perl")
            .args(["-e", "1"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false),
        "this test needs perl to build a setsid escapee"
    );
    /// Kills only the child marion started, leaving its setsid'd descendant alive — an
    /// incomplete sweep, by construction.
    fn kill_only_the_direct_child(pid: i32) {
        let _ = unsafe { kill(pid, SIGKILL) };
    }

    let dir = scratch("supervisor-drain-bound");
    let pids = dir.join("pids");
    // The escapee holds stdout open for 900 s and writes nothing, so the pipe stays open long
    // past every bound in this test.
    let script = format!(
        r#"use POSIX ();
               $| = 1;
               my $pid = fork();
               if ($pid == 0) {{
                   POSIX::setsid();
                   open(my $f, ">>", "{p}"); print $f "$$\n"; close $f;
                   sleep 900;
                   exit 0;
               }}
               print "before-the-bound\n";
               sleep 900;"#,
        p = pids.display()
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        let out = run_bounded_with(
            SysCommand::new("perl").args(["-e", &script]),
            StdDuration::from_millis(300),
            kill_only_the_direct_child,
            None,
            None,
        );
        let _ = tx.send(out);
    });
    // 300 ms bound + 2 s drain grace, with slack. Anything past this is the hang.
    let result = rx.recv_timeout(StdDuration::from_secs(10));

    // Unconditional cleanup first: no assertion below may leave a `sleep 900` behind.
    let escapees: Vec<i32> = std::fs::read_to_string(&pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    for p in &escapees {
        let _ = unsafe { kill(*p, SIGKILL) };
    }
    let _ = handle.join();

    let out = result
        .expect("run_bounded_with hung: a surviving escapee held the pipe open")
        .expect("run_bounded_with errored");
    assert!(out.timed_out, "the bound expired, so this is a timeout");
    assert!(
        out.capture_truncated,
        "the pipe was still open at the drain deadline, so the capture must be marked short"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "before-the-bound\n",
        "output drained before the bound is still returned"
    );
    assert_eq!(escapees.len(), 1, "expected one recorded escapee pid");
}

/// The abandoned drain is a stopped thread, not a wedged one: `finish` returns, and the thread
/// it was waiting on has exited by then. A supervisor calling `spawn` in a loop must not
/// accumulate one live thread and one live fd per timed-out spawn.
#[test]
fn a_drain_abandoned_with_the_pipe_still_open_stops_its_thread_rather_than_leaking_it() {
    // `exec`, so the shell *becomes* the sleeper: one process holds the pipe, and killing it
    // below leaves nothing behind even if an assertion fails first.
    let mut child = SysCommand::new("sh")
        .args(["-c", "echo drained; exec sleep 30"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let drain = Drain::start(child.stdout.take().expect("stdout was piped"));
    thread::sleep(StdDuration::from_millis(200));
    let stop = Arc::clone(&drain.stop);
    let started = Instant::now();
    // A deadline already in the past: abandon immediately.
    let (bytes, complete) = drain.finish(Instant::now());
    let elapsed = started.elapsed();
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        !complete,
        "the pipe was still open, so the capture is short"
    );
    assert_eq!(String::from_utf8_lossy(&bytes), "drained\n");
    assert!(
        stop.load(Ordering::Relaxed),
        "the thread was told to stop, not detached"
    );
    // `finish` joined the thread, so its return proves the thread exited and closed the fd.
    assert!(
        elapsed < StdDuration::from_secs(1),
        "abandoning a drain must be prompt, took {elapsed:?}"
    );
}

#[test]
fn a_child_that_closes_its_pipes_is_never_reported_as_truncated() {
    let out = run_bounded(
        SysCommand::new("sh").args(["-c", "echo out; echo err 1>&2"]),
        StdDuration::from_secs(10),
    )
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    assert!(!out.timed_out);
    assert!(!out.capture_truncated);
}

/// A drain must return every byte, not just the first pipe-buffer's worth: the chunked reader
/// has to loop. 512 KiB is far past the 64 KiB pipe capacity.
#[test]
fn output_larger_than_the_pipe_buffer_is_drained_whole() {
    let out = run_bounded(
        SysCommand::new("sh").args([
            "-c",
            "yes 0123456789012345678901234567890123456789 | head -n 12800",
        ]),
        StdDuration::from_secs(30),
    )
    .unwrap();
    assert_eq!(out.stdout.len(), 12800 * 41);
    assert!(!out.capture_truncated);
}

/// The live line seam sees every line the capture does — **while the child still runs**, on
/// the caller's thread, and the final unterminated line at EOF — and the capture is still
/// whole afterwards. Line one is printed and flushed before a sleep, so its arrival before the
/// child exits is what proves the seam is live rather than a replay of the capture.
#[test]
fn stdout_lines_reach_the_hook_while_the_child_runs_and_the_capture_stays_whole() {
    let seen: std::cell::RefCell<Vec<(String, bool)>> = std::cell::RefCell::new(Vec::new());
    let marker = scratch("supervisor-live-lines").join("exited");
    let script = format!(
        "printf 'first\\n'; sleep 0.4; printf 'second\\nthird-no-newline'; touch {}",
        marker.display()
    );
    let on_line = |line: &str| {
        seen.borrow_mut().push((line.to_string(), marker.exists()));
        ControlFlow::Continue(())
    };
    let out = run_bounded_with(
        SysCommand::new("sh").args(["-c", &script]),
        StdDuration::from_secs(30),
        kill_process_tree,
        None,
        Some(&on_line),
    )
    .unwrap();
    let seen = seen.into_inner();
    assert_eq!(
        seen.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
        ["first", "second", "third-no-newline"],
        "every line, the last one without its newline"
    );
    assert!(
        !seen[0].1,
        "`first` was delivered before the child had exited: the seam is live"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "first\nsecond\nthird-no-newline"
    );
    assert!(!out.capture_truncated);
}

#[test]
fn persisted_contract_is_complete_while_only_the_return_copy_is_capped() {
    let root = scratch("supervisor-persist");
    let project = ProjectDir::from_hash(&root, "0123456789ab");
    let agent = project.agent(&AgentId("agent".into()));
    let large = "x".repeat(20 * 1024);
    let contract = build_contract(
        TaskId("task".into()),
        AgentId("root".into()),
        marion_core::Harness::Codex,
        RepoIdentity {
            git_common_dir: Some("/r/.git".into()),
            head_branch: None,
        },
        Some(Oid("a".repeat(40))),
        Workspace::Worktree {
            path: "/wt".into(),
            branch: "b".into(),
        },
        "do it",
        &[],
        &[Glob("**".into())],
        &[Glob("**".into())],
        Duration::from_secs(1),
        SystemTime(std::time::SystemTime::now()),
        &ChildOutcome {
            narrative: Some(large),
            ..ChildOutcome::default()
        },
        Some(vec![]),
        None,
        vec![],
        vec![],
    );
    let returned = persist_then_cap(&agent, &contract).unwrap();
    let persisted: TaskContract =
        serde_json::from_slice(&std::fs::read(agent.contract(&contract.task_id)).unwrap()).unwrap();
    let persisted_narrative = persisted.completion.unwrap().narrative.unwrap();
    let returned_narrative = returned.completion.unwrap().narrative.unwrap();
    assert!(!persisted_narrative.truncated);
    assert!(returned_narrative.truncated);
    assert_eq!(persisted_narrative.original_bytes, 20 * 1024);
}

/// **A contract is replaced, never rewritten in place.** A reader holding the previous
/// document — or a crash between truncate and write — must see the old contract whole or the
/// new one whole, never an empty or half-written file. Observable as: a handle opened on the
/// old file still reads the old bytes after the new contract lands, and the new file is `0600`.
///
/// Mutation: write through `File::create` and the old handle reads the new (or a truncated)
/// document.
#[test]
fn a_rewritten_contract_replaces_the_old_file_rather_than_truncating_it() {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("supervisor-persist-atomic");
    let project = ProjectDir::from_hash(&root, "0123456789ab");
    let agent = project.agent(&AgentId("agent".into()));
    let task = TaskId("task".into());
    std::fs::create_dir_all(agent.contracts_dir()).unwrap();
    std::fs::write(agent.contract(&task), b"the old contract\n").unwrap();
    let mut old = std::fs::File::open(agent.contract(&task)).unwrap();
    let contract = build_contract(
        task.clone(),
        AgentId("root".into()),
        marion_core::Harness::Codex,
        RepoIdentity {
            git_common_dir: None,
            head_branch: None,
        },
        None,
        Workspace::Worktree {
            path: "/wt".into(),
            branch: "b".into(),
        },
        "do it",
        &[],
        &[Glob("**".into())],
        &[Glob("**".into())],
        Duration::from_secs(1),
        SystemTime(std::time::SystemTime::now()),
        &ChildOutcome::default(),
        Some(vec![]),
        None,
        vec![],
        vec![],
    );
    persist_then_cap(&agent, &contract).unwrap();
    let mut seen = String::new();
    old.read_to_string(&mut seen).unwrap();
    assert_eq!(seen, "the old contract\n");
    let path = agent.contract(&task);
    let _: TaskContract = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let strays: Vec<_> = std::fs::read_dir(agent.contracts_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != path.file_name().unwrap())
        .collect();
    assert!(strays.is_empty(), "no staging file survives: {strays:?}");
}

/// **A child whose `SpawnIntent` cannot be made durable is refused before its first side
/// effect.** The intent is what lets a restarted supervisor name the node; launching without it
/// is the untracked live process §9's M2 criteria forbid. The fault is real: the journal path
/// is a directory, so the append fails. No owner is told of an identity, no agent directory or
/// worktree exists, and no branch was made.
///
/// Mutation: write the intent through `journal::record` and the spawn goes on to take a
/// worktree (and `identified` fires).
#[test]
fn a_child_whose_intent_cannot_be_journalled_is_refused_before_any_side_effect() {
    struct Counter(Mutex<Vec<AgentId>>);
    impl SpawnObserver for Counter {
        fn identified(&self, agent_id: &AgentId) -> Option<Secret> {
            self.0.lock().unwrap().push(agent_id.clone());
            None
        }
        fn started(&self, _: &AgentId, _: i32) {}
    }
    let root = scratch("supervisor-intent-barrier");
    let repo = fixture_repo(&root);
    let state = root.join("state");
    let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
    std::fs::create_dir_all(project.journal()).unwrap();
    let env = Env {
        os_sandbox: true,
        project_dir: project.clone(),
        state: state.clone(),
        project_root: repo.clone(),
        bridge: PathBuf::from("/bin/marion-supervisor"),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        auth: Auth::Canned,
    };
    let req = SpawnRequest {
        budget: None,
        review: None,
        race: None,
        agent_type: "codex".into(),
        prompt: "do the task".into(),
        repo: repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: 1,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        read_only: false,
        workflow: None,
    };
    let observer = Counter(Mutex::default());
    let e = run_spawn_watched(
        &env,
        &req,
        &TaskId("intent-barrier".into()),
        &Requester::from(Caller::root("root", builtin("codex").unwrap())),
        &observer,
    )
    .expect_err("no durable intent, no child");
    assert!(
        matches!(e, SpawnError::SpawnIntentBarrier { .. }),
        "the wrong refusal: {e}"
    );
    assert!(
        observer.0.lock().unwrap().is_empty(),
        "no identity announced"
    );
    let agents: Vec<_> = std::fs::read_dir(project.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "journal.jsonl")
        .collect();
    assert!(agents.is_empty(), "no agent directory: {agents:?}");
    let branches = marion_testsupport::git(&repo, &["branch", "--list", "marion/*"]);
    assert!(branches.trim().is_empty(), "no task branch: {branches}");
}

/// **A finished child's task branch is removed only while it still points at the base marion
/// created it at.** An unchanged branch is residue that would make the task id single-use
/// (`git worktree add -b` cannot recreate it); a branch that moved holds work and survives.
/// The deletion is git's compare-and-delete, so the check and the delete are one step.
///
/// Mutation: drop the `update-ref -d` and the unchanged branch survives; drop its old-value
/// argument and the advanced branch (and the only name for its commit) is deleted.
#[test]
fn cleanup_deletes_an_unchanged_task_branch_and_keeps_an_advanced_one() {
    let root = scratch("run-cleanup-branch");
    let repo = fixture_repo(&root);
    let git = |dir: &Path, args: &[&str]| marion_testsupport::git(dir, args);
    let exists = |branch: &str| {
        SysCommand::new("git")
            .current_dir(&repo)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .output()
            .unwrap()
            .status
            .success()
    };

    let unchanged = root.join("wt-unchanged");
    let (base, _) = crate::spawn::make_worktree(
        crate::spawn::Tree::Operator(&repo),
        &unchanged,
        &["marion/unchanged".into()],
        false,
    )
    .unwrap();
    cleanup(&repo, &unchanged, "marion/unchanged", Some(&base));
    assert!(!unchanged.exists(), "the worktree is removed");
    assert!(
        !exists("marion/unchanged"),
        "and its unchanged branch with it"
    );
    crate::spawn::make_worktree(
        crate::spawn::Tree::Operator(&repo),
        &unchanged,
        &["marion/unchanged".into()],
        false,
    )
    .expect("so the task id can be used again");

    let advanced = root.join("wt-advanced");
    let (base, _) = crate::spawn::make_worktree(
        crate::spawn::Tree::Operator(&repo),
        &advanced,
        &["marion/advanced".into()],
        false,
    )
    .unwrap();
    git(
        &advanced,
        &[
            "-c",
            "user.email=m@example.invalid",
            "-c",
            "user.name=m",
            "commit",
            "--allow-empty",
            "-qm",
            "work",
        ],
    );
    cleanup(&repo, &advanced, "marion/advanced", Some(&base));
    assert!(!advanced.exists(), "the worktree is removed");
    assert!(
        exists("marion/advanced"),
        "but a branch holding work is kept"
    );

    assert_eq!(
        task_branch_ref("marion/0197f3aa-1c2d"),
        Some("refs/heads/marion/0197f3aa-1c2d".into())
    );
    for branch in [
        "main",
        "marion/",
        "marion/a/b",
        "marion/..",
        "refs/heads/main",
    ] {
        assert_eq!(task_branch_ref(branch), None, "{branch:?}");
    }
}

/// **A spawn refused after its worktree exists takes the worktree back.** The worktree is the
/// first irreversible thing a spawn does, and several refusals can still follow it before any
/// process runs — here the protocol row with no agent bound, which has no child surface to
/// launch. Each one used to return with the directory, its `.git/worktrees/` registration and
/// the unchanged `marion/<task>` branch left in the operator's repository.
///
/// Mutation: drop the prelaunch guard (or disarm it before the refusal) and all three remain.
#[test]
fn a_spawn_refused_after_its_worktree_exists_leaves_no_worktree_or_branch() {
    let root = scratch("run-prelaunch-worktree");
    let repo = fixture_repo(&root);
    let types = repo.join(AGENT_TYPES_FILE);
    std::fs::create_dir_all(types.parent().unwrap()).unwrap();
    std::fs::write(
        &types,
        "[[agent]]\nname = \"bare-acp\"\nharness = \"acp\"\ndescription = \"No agent.\"\n",
    )
    .unwrap();
    let state = root.join("state");
    let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
    let env = Env {
        os_sandbox: false,
        project_dir: project.clone(),
        state: state.clone(),
        project_root: repo.clone(),
        bridge: PathBuf::from("/bin/marion-supervisor"),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        auth: Auth::Canned,
    };
    let req = SpawnRequest {
        budget: None,
        review: None,
        race: None,
        agent_type: "bare-acp".into(),
        prompt: "do the task".into(),
        repo: repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: 1,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        read_only: false,
        workflow: None,
    };
    let e = run_spawn_watched(
        &env,
        &req,
        &TaskId("prelaunch".into()),
        // Unsandboxed, so the containment gate is not what refuses this spawn.
        &Requester::from(Caller::root("root", builtin("claude").unwrap())),
        &Unwatched,
    )
    .expect_err("the protocol row with no agent cannot be launched");
    assert!(
        matches!(e, SpawnError::Harness(_)),
        "refused by the adapter, after the worktree: {e}"
    );
    let branches = marion_testsupport::git(&repo, &["branch", "--list", "marion/*"]);
    assert!(branches.trim().is_empty(), "no task branch: {branches}");
    let worktrees = marion_testsupport::git(&repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1,
        "only the operator's own checkout is registered: {worktrees}"
    );
    let node = crate::registry::Registry::boot(&project)
        .unwrap()
        .tree()
        .nodes()
        .iter()
        .map(|n| n.agent_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(node.len(), 1, "one node was intended: {node:?}");
    assert!(
        !project.agent(&node[0]).worktree().exists(),
        "no worktree directory"
    );
}

/// A short capture that reads as a whole one is the invisible failure design §6.7 exists to
/// prevent, so the timeout description has to carry it — and has to keep the timeout wording.
#[test]
fn a_drain_bound_truncation_is_recorded_in_the_exit_description() {
    let noted =
        note_truncated_capture("child exceeded its timeout and its process group was killed");
    assert!(noted.starts_with("child exceeded its timeout"));
    assert!(noted.contains("output capture truncated"));
}

/// **A branch name says whose and what for, in a screen's width**: the row's short id, then
/// the prompt's first words as a ref-safe slug; the short id alone where the prompt has no
/// ASCII word.
#[test]
fn a_worktree_branch_is_the_short_id_and_the_tasks_first_words() {
    let agent = AgentId("01a0e64e-433f-7ed8-aa38-c369b4e5918c".into());
    assert_eq!(
        worktree_branch(
            &agent,
            "Add a --top N option to wordfreq.py that prints only the N most frequent words"
        ),
        "marion/433f-add-a-top-n-option-to"
    );
    assert_eq!(worktree_branch(&agent, "  ¿qué?  "), "marion/433f-qu");
    assert_eq!(worktree_branch(&agent, "日本語"), "marion/433f");
    assert_eq!(
        slug("Supercalifragilisticexpialidocious words"),
        "supercalifragilisticexpi"
    );
    assert_eq!(slug("fix: CI.lock ../..//x"), "fix-ci-lock-x");
    for p in ["", "a", "Add a limiter", "x".repeat(100).as_str()] {
        let b = worktree_branch(&agent, p);
        assert!(b.len() <= "marion/433f-".len() + SLUG_CHARS, "{b}");
        let ok = std::process::Command::new("git")
            .args(["check-ref-format", "--branch", &b])
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "{b} is a valid branch name");
    }
    assert_eq!(
        legacy_worktree_branch(&TaskId("01a0e64e-433c".into())),
        "marion/01a0e64e-433c"
    );
}

/// **A readable name already taken falls back to the task's own id**, which no other task
/// shares, rather than failing the spawn or reusing another node's branch.
#[test]
fn a_taken_branch_name_falls_back_to_the_task_id() {
    let s = scratch("worktree-branch-taken");
    let repo = fixture_repo(&s);
    let names = |t: &str| vec!["marion/433f-add".to_string(), format!("marion/{t}")];
    let (_, first) = crate::spawn::make_worktree(
        crate::spawn::Tree::Operator(&repo),
        &s.join("wt1"),
        &names("t-1"),
        false,
    )
    .unwrap();
    assert_eq!(first, "marion/433f-add");
    let (_, second) = crate::spawn::make_worktree(
        crate::spawn::Tree::Operator(&repo),
        &s.join("wt2"),
        &names("t-2"),
        false,
    )
    .unwrap();
    assert_eq!(second, "marion/t-2");
    assert_eq!(
        crate::spawn::checked_out_branch(crate::spawn::Tree::Operator(&s.join("wt2"))).as_deref(),
        Some("marion/t-2")
    );
    assert_eq!(
        crate::spawn::checked_out_branch(crate::spawn::Tree::Operator(&s.join("gone"))),
        None
    );
}

/// A repo with one commit, which is all `make_worktree` needs to get past `rev-parse HEAD`.
fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/keep.txt"), "keep\n").unwrap();
    for args in [
        vec!["init", "-q", "-b", "main", "."],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.email=marion@example.invalid",
            "-c",
            "user.name=marion",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        let out = SysCommand::new("git")
            .current_dir(&repo)
            .args(&args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    repo
}

/// Every file under `dir`, recursively. Used to prove a *negative* — that nothing anywhere
/// under the node's state is a Codex `config.toml` — because asserting the absence of one
/// guessed path would pass if the adapter simply wrote it somewhere else.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// **The dispatch regression.** `run_spawn` selects its adapter from `agent_type.harness`; for
/// as long as it selected `Harness::Codex` by constant, spawning the `claude` type wrote a
/// Codex `config.toml`, exec'd `codex`, and returned a contract stamped `"claude-code"` — an
/// audit record (§6.7) describing a run that never happened.
///
/// Two assertions, and each one fails against the constant:
/// 1. no `config.toml` exists anywhere under the node's state — the Codex adapter's one output;
/// 2. whatever the run produced, it is **the Claude Code adapter's**: its MCP declaration on
///    disk, and `claude-code` in the contract.
///
/// **Re-pointed twice, never weakened, and the note has always said which part is stable.**
/// The original asserted a typed `MissingInput` naming `claude-code`, because `run_spawn` built
/// a `SpawnCtx` with `ready_file: None`; the second revision asserted the argv-prompt launch
/// that replaced it. Both notes said the same thing — *"this test asserts the harness it names,
/// not the branch it took"*. The branch has moved again, to the one this harness's surfaces
/// actually declare: a `claude` child is a **duplex** node, so §6.1 step 8's readiness gate now
/// runs on it. Against a base URL nobody serves and a one-second bound the gate is what fires,
/// and that refusal is itself proof the duplex path ran — no other path has a marker to wait on.
///
/// So the acceptable outcomes are exactly the claude-code-shaped ones, and the arm that would
/// have caught the original bug is untouched: a contract stamped with any other harness, or a
/// Codex `config.toml` on disk, still fails.
#[test]
fn a_claude_agent_type_never_writes_a_codex_config_or_launches_codex() {
    if !marion_testsupport::harness_available("claude") {
        return;
    }
    let root = scratch("supervisor-dispatch");
    let repo = fixture_repo(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = Env {
        os_sandbox: true,
        project_dir: ProjectDir::new(&state, &repo),
        state: state.clone(),
        project_root: repo.clone(),
        bridge: PathBuf::from("/bin/marion-supervisor"),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        auth: Auth::Canned,
    };
    let req = SpawnRequest {
        budget: None,
        review: None,
        race: None,
        agent_type: "claude".into(),
        prompt: "do the task".into(),
        repo: repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        // Short: the base URL below answers nothing, so this bounds the launch to a second —
        // and a regression re-running `codex` for real is a fast failure rather than a wait.
        timeout_secs: 1,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        read_only: false,
        workflow: None,
    };

    let result = run_spawn(
        &env,
        &req,
        &TaskId("dispatch".into()),
        &Caller::root("root", builtin("claude").unwrap()),
    );

    let mut written = Vec::new();
    files_under(&state, &mut written);
    assert!(
        !written
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == "config.toml")),
        "a Codex config.toml was written for a claude agent type: {written:?}"
    );
    assert!(
        written
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == "mcp.json")),
        "the Claude Code adapter's own output must be what is on disk: {written:?}"
    );
    match result {
        Ok(c) => assert_eq!(
            c.child.harness,
            Harness::ClaudeCode,
            "the contract must name the harness that ran"
        ),
        Err(e) => assert!(
            matches!(
                &e,
                SpawnError::Harness(marion_harness::HarnessError::MissingInput {
                    harness: Harness::ClaudeCode,
                    ..
                })
            ) | matches!(
                &e,
                SpawnError::Duplex(crate::duplex::DuplexError::McpNeverReady(_, _))
                    | SpawnError::Duplex(crate::duplex::DuplexError::DiedBeforeInitialize)
            ),
            "a refusal is still an acceptable outcome — but a typed one belonging to the \
             harness that was asked for, never a fallback onto another. §6.1 step 8's gate \
             only exists on the duplex path, so its refusal names that path as surely as \
             MissingInput names the adapter. Got: {e}"
        ),
    }
}

/// **The two instants an owner outside `run_spawn` has to be told about**, and the fact that
/// each is told *before* the thing it is about becomes irreversible.
///
/// §11 item 28 step 4 moves node ownership into the supervisor, and an owner that learns a
/// node's identity only from `run_spawn`'s **return** has learned it after the node has run.
/// Two hooks, and the ordering of each is the whole assertion:
///
/// * `identified` fires once the `SpawnIntent` is on disk and **before any side effect** — no
///   worktree, no config document, no process. That is what lets it mint §5.4's token in time
///   for the declaration this test then reads off disk. Firing it later would put the token in
///   a file already written; firing it before the intent would name a node no restart could
///   find.
/// * `started` fires at `command.spawn()`, carrying a real pid. It is the same instant
///   `Spawned` is journaled (step 1), so an owner that returns when this fires is making a
///   claim the journal already backs rather than one it is about to.
///
/// The token is asserted **on the bytes marion wrote**, not on what the observer returned: the
/// value only means anything if it reached the one file the node's own bridge reads. A run
/// whose observer was consulted and whose answer was dropped would pass every other assertion
/// here.
///
/// The run itself is allowed to fail — the base URL answers nothing and the bound is a second,
/// exactly as the dispatch test above arranges. What is asserted is what happened *before* it
/// failed.
#[test]
fn an_owner_learns_a_nodes_identity_before_its_first_side_effect_and_its_pid_at_launch() {
    if !marion_testsupport::harness_available("claude") {
        return;
    }
    struct Recorder {
        project: ProjectDir,
        identified: Mutex<Vec<AgentId>>,
        /// What the world looked like **at the instant `identified` fired** — recorded from
        /// inside the hook, because no assertion afterwards can see that instant. Pairs of
        /// (the intent is already durable, a side effect has already been taken).
        at_identify: Mutex<Vec<(bool, bool)>>,
        started: Mutex<Vec<(AgentId, i32)>>,
    }
    const TOKEN: &str = "MARION-OBSERVER-TOKEN-b17f";
    impl SpawnObserver for Recorder {
        fn identified(&self, agent_id: &AgentId) -> Option<Secret> {
            // Read back through the same replay a restarted supervisor would use, not by
            // grepping the file: what matters is that a *reader* can find the node.
            let intent_durable = crate::registry::Registry::boot(&self.project)
                .map(|r| r.tree().get(agent_id).is_some())
                .unwrap_or(false);
            let side_effect_taken = self.project.agent(agent_id).config_dir().exists();
            self.at_identify
                .lock()
                .unwrap()
                .push((intent_durable, side_effect_taken));
            self.identified.lock().unwrap().push(agent_id.clone());
            Some(TOKEN.into())
        }
        fn started(&self, agent_id: &AgentId, pid: i32) {
            self.started.lock().unwrap().push((agent_id.clone(), pid));
        }
    }

    let root = scratch("supervisor-observer");
    let repo = fixture_repo(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = Env {
        os_sandbox: true,
        project_dir: ProjectDir::new(&state, &repo),
        state: state.clone(),
        project_root: repo.clone(),
        bridge: PathBuf::from("/bin/marion-supervisor"),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        auth: Auth::Canned,
    };
    let req = SpawnRequest {
        budget: None,
        review: None,
        race: None,
        agent_type: "claude".into(),
        prompt: "do the task".into(),
        repo: repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: 1,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        read_only: false,
        workflow: None,
    };
    let observer = Recorder {
        project: env.project_dir.clone(),
        identified: Mutex::default(),
        at_identify: Mutex::default(),
        started: Mutex::default(),
    };
    let _ = run_spawn_watched(
        &env,
        &req,
        &TaskId("observer".into()),
        &Requester::from(Caller::root("root", builtin("claude").unwrap())),
        &observer,
    );

    let identified = observer.identified.lock().unwrap().clone();
    assert_eq!(
        identified.len(),
        1,
        "exactly one node was spawned, so exactly one identity was announced"
    );
    // **The position of the hook, asserted rather than described.** Both halves fail against a
    // different placement: announcing before the intent is journaled gives an owner a node no
    // restart could find, and announcing after the first side effect gives it a token decided
    // too late for the document that has to carry it.
    assert_eq!(
        observer.at_identify.lock().unwrap().as_slice(),
        &[(true, false)],
        "at the instant the owner is told, the SpawnIntent must be durable (first) and no side \
         effect taken (second)"
    );
    let started = observer.started.lock().unwrap().clone();
    assert_eq!(started.len(), 1, "one process, one pid: {started:?}");
    assert_eq!(
        started[0].0, identified[0],
        "the pid must be announced for the node whose identity was announced, or an owner \
         keyed by AgentId files it under a node that does not exist"
    );
    assert!(
        started[0].1 > 0,
        "a pid a signal could reach, not a placeholder: {}",
        started[0].1
    );

    // The identity marion told the owner is the identity marion journaled. Reading it back off
    // the tree rather than trusting the hook is what stops a hook that announces some *other*
    // node's id from passing.
    let journalled: Vec<_> = crate::registry::Registry::boot(&env.project_dir)
        .expect("the journal this run wrote is readable")
        .tree()
        .nodes()
        .iter()
        .map(|n| n.agent_id.clone())
        .collect();
    assert!(
        journalled.contains(&identified[0]),
        "the announced identity must be the journalled one: announced {identified:?}, \
         journalled {journalled:?}"
    );

    // And the token reached the one file the node's own bridge reads.
    let mut written = Vec::new();
    files_under(&state, &mut written);
    let mcp = written
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "mcp.json"))
        .unwrap_or_else(|| panic!("the adapter wrote no declaration: {written:?}"));
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(mcp).unwrap()).unwrap();
    assert_eq!(
        doc["mcpServers"]["marion"]["env"][marion_harness::claude_code::NODE_TOKEN_ENV],
        serde_json::json!(TOKEN),
        "the token the owner minted must be in the declaration, beside MARION_AGENT_ID — a \
         token nobody wrote down is a capability nothing can present:\n{doc:#}"
    );
    assert_eq!(
        doc["mcpServers"]["marion"]["env"][marion_harness::claude_code::AGENT_ID_ENV],
        serde_json::json!(identified[0].0),
        "…and beside the identity it is bound to"
    );
}

/// The other half of item 1: the Codex path still resolves to the Codex adapter, so the
/// dispatch change is observably a no-op for every M1 spawn.
#[test]
fn each_builtin_agent_type_dispatches_to_its_own_harness() {
    for (name, expected) in [
        ("claude", Harness::ClaudeCode),
        ("claude-impl", Harness::ClaudeCode),
        ("codex", Harness::Codex),
        ("codex-impl", Harness::Codex),
        ("opencode", Harness::OpenCode),
        ("copilot", Harness::Copilot),
        ("copilot-impl", Harness::Copilot),
        ("goose", Harness::Goose),
        ("goose-impl", Harness::Goose),
        ("cline", Harness::Cline),
        ("qwen", Harness::Qwen),
        ("qwen-impl", Harness::Qwen),
        ("pi", Harness::Pi),
        ("pi-orchestrator", Harness::Pi),
    ] {
        let t = builtin(name).expect("built-in resolves");
        assert_eq!(t.harness, expected, "{name}");
        assert_eq!(
            adapter_for(t.harness)
                .expect("every built-in's harness has an adapter")
                .harness(),
            expected,
            "{name}: the adapter run_spawn selects must be this type's harness"
        );
    }
}

/// **Re-pointed, not weakened.** This test used to reach the refusal through `adapter_for`,
/// because some harnesses had no adapter. They do now, so that route is gone — and
/// asserting it against some other harness would have been vacuous, since the registry is
/// exhaustive over `Harness::ALL` (`marion-harness::adapter::…resolves_to_an_adapter`). What
/// the test was actually defending is the *conversion*: whatever produces an `Unimplemented`,
/// `run_spawn`'s `?` must surface it as a typed `SpawnError` naming the harness, never as a
/// fallback or a flattened string. That is asserted here directly, for every harness.
#[test]
fn an_unimplemented_harness_reaches_run_spawn_as_a_refusal_that_names_it() {
    for h in Harness::ALL {
        let err: SpawnError = marion_harness::HarnessError::Unimplemented(h).into();
        assert!(
            matches!(
                err,
                SpawnError::Harness(marion_harness::HarnessError::Unimplemented(g)) if g == h
            ),
            "{h}: expected a typed Unimplemented refusal"
        );
        assert!(
            err.to_string().contains(h.as_str()),
            "{h}: the message must name the harness, got {err}"
        );
    }
}

fn launch_ctx() -> SpawnCtx {
    SpawnCtx {
        agent_id: AgentId("019f-child".into()),
        agent_type: "codex-impl".into(),
        depth: 1,
        node_token: None,
        ready_file: None,
        repo: "/repo".into(),
        state_dir: "/state".into(),
        bridge: "/bin/marion-supervisor".into(),
        bridge_args: vec!["mcp".into()],
    }
}

fn launch_spec(model: Option<String>) -> LaunchSpec {
    LaunchSpec {
        cwd: "/wt".into(),
        model,
        prompt: "do the task".into(),
        tools: vec![],
        allowed_tools: vec![],
        mcp: McpDeclaration::Marion,
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        api_key: None,
        auth: Auth::Canned,
        config_dir: "/state/x/config".into(),
        resume: None,
        wire: None,
        provider: None,
        extra: Extras::default(),
    }
}

/// **The child's two axes, composed the way `run_spawn` composes them.**
///
/// `run_spawn` sets `tools` from the resolved agent type and `allowed_tools` to marion's verbs
/// for the child's depth (`agent_type::child_verbs`), and leaves the union to the adapter.
/// This drives that exact pair through the exact adapter the dispatch above selects, so the
/// composition is checked without a process: a `claude-impl` child gets `Write` on **both**
/// flags and marion's own verbs are not lost from the permission axis in the process.
///
/// **It restates two lines of `run_spawn` rather than calling them, and that is stated rather
/// than hidden.** `run_spawn` needs a repo, a worktree and a process, so the wiring itself is
/// pinned end to end by the harness cross-product's writing cells; what this catches is the
/// composition being wrong — the union done at the call site instead of in the adapter (which
/// would grant permission without availability), or `report` dropped while unioning (which
/// would leave a child that can write and cannot report, the §12 shape in a new place).
#[test]
fn a_child_of_an_impl_type_is_compiled_with_availability_and_permission_open_together() {
    let t = builtin("claude-impl").expect("the implementer type resolves");
    let adapter = adapter_for(t.harness).unwrap();
    let spec = LaunchSpec {
        // Verbatim from `run_spawn`, for a child at depth 1.
        tools: t.tools.clone(),
        allowed_tools: marion_core::agent_type::child_verbs(&t, 1)
            .into_iter()
            .map(|verb| adapter.marion_tool_name(verb))
            .collect(),
        // A duplex child's prompt is a frame written after launch, so argv carries none.
        prompt: String::new(),
        ..launch_spec(None)
    };
    let args = adapter.compile(&spec, &launch_ctx()).unwrap().args;
    let after = |flag: &str| -> String {
        let i = args.iter().position(|a| a == flag).expect("flag present");
        args[i + 1].clone()
    };
    assert_eq!(after("--tools"), "Read,Write,Edit,Bash", "availability");
    assert_eq!(
        after("--allowedTools"),
        "mcp__marion__report,mcp__marion__spawn,mcp__marion__status,mcp__marion__wait,\
         mcp__marion__list,mcp__marion__steer,mcp__marion__cancel,Read,Write,Edit,Bash",
        "permission carries marion's verbs AND the whole declaration; either alone is a dead \
         end, and a verb that reached availability and not permission is item 22's"
    );
    // The orchestrator type through the same path: unchanged, which is what keeps this
    // additive.
    let orchestrator = LaunchSpec {
        tools: builtin("claude-orchestrator").unwrap().tools,
        ..spec
    };
    let args = adapter.compile(&orchestrator, &launch_ctx()).unwrap().args;
    let i = args.iter().position(|a| a == "--tools").unwrap();
    assert_eq!(
        args[i + 1],
        "",
        "a `claude-orchestrator` child is read-only, as plain `claude` used to be"
    );
}

/// **§6.7's `allowed_tools` is the compiled constraint, never the requested one — and codex is
/// where those two are visibly different strings.**
///
/// A `codex-impl` node asked for marion's `write`; what codex actually ran under is
/// `sandbox:workspace-write`, because `codex exec` has no allowlist to check a call against.
/// Recording the request would put marion's own vocabulary in a field §3.1 says must never
/// carry it: *"echoing marion's own vocabulary there would make the field claim a constraint
/// that never existed."*
///
/// The four harnesses answer in four different shapes, which is the other half of the claim: a
/// uniform answer is what the hardcoded `["apply_patch", "shell"]` was, and it was wrong on all
/// four. Driven through `adapter_for` exactly as `run_spawn` drives it; the assignment into the
/// contract is one line beside `child.harness` and `child.model`, and is pinned end to end by
/// the harness cross-product.
#[test]
fn the_contract_records_the_compiled_constraint_and_never_the_requested_tool() {
    // Every harness driven with the SAME request — marion's `write` — so what differs in the
    // column below is only how each harness expresses the constraint. Declared explicitly
    // rather than read off a built-in: `codex-impl` and `opencode` state no tools, so a
    // built-in-only sweep could never put `write` in front of those two adapters and the
    // "never the requested word" assertion would pass vacuously on the two harnesses where it
    // is most likely to be violated.
    let declared = vec![marion_core::agent_type::TOOL_WRITE.to_string()];
    for (harness, want) in [
        (Harness::ClaudeCode, vec!["mcp__marion__report", "Write"]),
        // **The discriminating cell.** `write` in, `sandbox:workspace-write` out: the request
        // and the compiled constraint are visibly different strings, so a record sourced from
        // the request cannot pass here by coincidence the way it could where the two agree.
        (Harness::Codex, vec!["sandbox:workspace-write"]),
        (Harness::OpenCode, vec!["harness-default:unconstrained"]),
        // The pattern grammar, not the tool grammar: `write` is the kind that grants `create`.
        (
            Harness::Copilot,
            vec!["allow-tool:marion(report)", "allow-tool:write"],
        ),
        // The builtin grammar: `write` is one of the developer extension's tools, and the
        // extension is the unit goose grants.
        (Harness::Goose, vec!["with-builtin:developer"]),
        // opencode's record, for opencode's reason: the 26 built-ins are offered regardless.
        (Harness::Cline, vec!["harness-default:unconstrained"]),
        // The `--core-tools` list itself: marion's verb, then the declared built-in.
        (
            Harness::Qwen,
            vec!["core-tools:mcp__marion__report", "core-tools:write_file"],
        ),
        // The `--tools` list itself: marion's verb, then both built-ins that change a file.
        (
            Harness::Pi,
            vec!["tools:mcp__marion__report", "tools:write", "tools:edit"],
        ),
    ] {
        let adapter = adapter_for(harness).unwrap();
        let launch = LaunchSpec {
            tools: declared.clone(),
            allowed_tools: vec![adapter.marion_tool_name("report")],
            prompt: String::new(),
            model: Some("m/m".into()),
            ..launch_spec(None)
        };
        let recorded = adapter.compiled_permissions(&launch).unwrap();
        assert_eq!(recorded, want, "{harness}");
        assert!(
            !recorded.iter().any(|r| declared.contains(r)),
            "{harness}: `write` is marion's word for the request, not any harness's word for \
             the constraint — recording it would be the request masquerading as the outcome. \
             Got: {recorded:?}"
        );
    }
}

fn request(agent_type: &str, model: Option<&str>) -> SpawnRequest {
    SpawnRequest {
        budget: None,
        review: None,
        agent_type: agent_type.into(),
        prompt: "do the task".into(),
        // A placeholder, overwritten by every caller that actually launches: `resolve_model`
        // is pure and never reaches a filesystem, so a real tree here would be scenery.
        repo: PathBuf::from("/repo"),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec![],
        timeout_secs: 1,
        model: model.map(str::to_string),
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        race: None,
        read_only: false,
        workflow: None,
    }
}

/// **The gap this phase closed.** `run_spawn` used to build its `LaunchSpec` with a hard
/// `model: None`, and §6.4 makes an explicit model a MUST on the harnesses that need one (opencode
/// has no `OPENCODE_MODEL` env var; copilot's BYOK refuses to start without one) — so an
/// `opencode` or `copilot` agent type could be named, resolved and dispatched, and then refused at `compile`. The
/// resolved model is what makes them launchable, so the test asserts the *whole chain*: the
/// built-in's default reaches `resolve_model`, and what `resolve_model` returns compiles.
#[test]
fn the_new_harnesses_now_compile_because_their_agent_types_carry_a_model() {
    for name in ["opencode", "copilot"] {
        let t = builtin(name).unwrap();
        let model = resolve_model(&request(name, None), &t, Auth::Canned);
        assert!(
            model.is_some(),
            "{name}: its adapter refuses without one, so its built-in must state one"
        );
        adapter_for(t.harness)
            .unwrap()
            .compile(&launch_spec(model), &launch_ctx())
            .unwrap_or_else(|e| panic!("{name} still cannot compile: {e}"));
    }
}

/// And the refusal is still there for anyone who defeats the default: it names its own harness
/// rather than falling through to codex.
#[test]
fn a_new_harness_with_no_model_anywhere_still_refuses_and_names_itself() {
    for (name, h) in [
        ("opencode", Harness::OpenCode),
        ("copilot", Harness::Copilot),
        ("goose", Harness::Goose),
        ("cline", Harness::Cline),
        ("qwen", Harness::Qwen),
        ("pi", Harness::Pi),
    ] {
        let adapter = adapter_for(builtin(name).unwrap().harness).unwrap();
        let err: SpawnError = adapter
            .compile(&launch_spec(None), &launch_ctx())
            .unwrap_err()
            .into();
        assert!(
            matches!(
                err,
                SpawnError::Harness(marion_harness::HarnessError::MissingInput {
                    harness: g,
                    ..
                }) if g == h
            ),
            "{name}: expected a typed refusal naming {h}, got {err}"
        );
    }
}

/// §3.1's precedence, in one statement: the request wins, the agent type is the default, and
/// the two harnesses that have always run without a model still resolve to `None` — which is
/// what keeps `codex exec`'s measured argv, and `cross_product` and `timeout_kill` with it, unchanged.
#[test]
fn the_request_overrides_the_agent_types_default_and_absence_stays_absence() {
    let copilot = builtin("copilot").unwrap();
    assert_eq!(
        resolve_model(
            &request("copilot", Some("gpt-5-codex")),
            &copilot,
            Auth::Canned
        )
        .as_deref(),
        Some("gpt-5-codex"),
    );
    assert_eq!(
        resolve_model(&request("copilot", None), &copilot, Auth::Canned).as_deref(),
        Some(marion_core::agent_type::COPILOT_DEFAULT_MODEL),
    );
    for name in ["codex-impl", "claude"] {
        assert_eq!(
            resolve_model(&request(name, None), &builtin(name).unwrap(), Auth::Canned),
            None,
            "{name}: a default here would change an argv that is measured, for nothing"
        );
    }
}

/// **A live child of a type whose default names marion's canned plumbing runs on the
/// operator's own default model**: `marion/default` names a provider block only a canned run
/// writes, so passing it live made every opencode child fail at launch.
#[test]
fn a_live_spawn_does_not_inherit_a_canned_plumbing_default_model() {
    let opencode = builtin("opencode").unwrap();
    assert_eq!(
        resolve_model(&request("opencode", None), &opencode, Auth::Inherited),
        None
    );
    assert_eq!(
        resolve_model(&request("opencode", None), &opencode, Auth::Canned).as_deref(),
        Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL)
    );
    assert_eq!(
        resolve_model(
            &request("opencode", Some("a/b")),
            &opencode,
            Auth::Inherited
        )
        .as_deref(),
        Some("a/b"),
        "an explicit model is always honoured"
    );
}

/// **The contract records the wire, not the ask** — the same rule that made `child.harness`
/// come from the adapter. A caller can name a model for a codex child; `codex exec` carries
/// none, so the contract must not claim one.
#[test]
fn a_model_asked_for_on_a_harness_that_takes_none_is_never_recorded_as_used() {
    let t = builtin("codex-impl").unwrap();
    let asked = resolve_model(
        &request("codex-impl", Some("gpt-5.6-sol")),
        &t,
        Auth::Canned,
    );
    assert_eq!(
        asked.as_deref(),
        Some("gpt-5.6-sol"),
        "the ask is honoured…"
    );
    let inv = adapter_for(t.harness)
        .unwrap()
        .compile(&launch_spec(asked), &launch_ctx())
        .unwrap();
    assert_eq!(
        inv.model, None,
        "…but nothing carried it, so `child.model` records nothing"
    );
    assert!(
        !inv.args.iter().any(|a| a == "gpt-5.6-sol"),
        "and it reached no argv either"
    );
}

/// An `Env`, a state dir and a fixture repo, for the tests that call `run_spawn` for real.
///
/// The repo is returned separately because it is no longer part of the environment: it is a
/// per-spawn input, so each of these tests states it on its own request.
/// **The file is read from the working tree the spawn is against, at every spawn.** No file is
/// the built-in table; a file that cannot be parsed is a refusal that names the file and the
/// reason, distinct from an unknown type — the operator's fix is in the file, not the request.
#[test]
fn agent_types_reads_the_trees_file_or_refuses_by_name() {
    let root = scratch("supervisor-agent-types");
    let repo = fixture_repo(&root);
    assert_eq!(
        agent_types(&repo).unwrap(),
        marion_core::agent_type::AgentTypes::builtins_only(),
        "no file: the built-ins"
    );
    let file = repo.join(AGENT_TYPES_FILE);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"Reviews.\"\n",
    )
    .unwrap();
    let types = agent_types(&repo).unwrap();
    assert_eq!(
        types.resolve("reviewer").unwrap().harness,
        marion_core::harness::Harness::Codex
    );
    std::fs::write(
        &file,
        "[[agent]]\nname = \"codex\"\nharness = \"codex\"\ndescription = \"x\"\n",
    )
    .unwrap();
    let e = agent_types(&repo).unwrap_err();
    match &e {
        SpawnError::AgentTypesFile { path, .. } => assert_eq!(path, &file),
        other => panic!("a broken file is its own refusal, got {other:?}"),
    }
    let msg = e.to_string();
    assert!(msg.contains(&file.display().to_string()), "{msg}");
    assert!(
        msg.contains("shadow"),
        "the parser's reason survives: {msg}"
    );
    // A directory where the file should be is an io error, not "no file".
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert!(
        matches!(agent_types(&repo), Err(SpawnError::AgentTypesFile { .. })),
        "only NotFound means the built-ins"
    );
}

/// **A node's config documents are the owner's alone.** They carry the node's capability token
/// in the bridge declaration and, on an endpoint node, the user's key (opencode's provider
/// block, cline's `providers.json`), so no other user on the machine may read them.
#[test]
fn config_documents_are_written_owner_only_even_over_a_wider_file() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("config-docs-mode");
    let path = root.join("nested/dir/doc.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let fresh = root.join("fresh/doc.toml");
    write_config_documents(vec![
        (path.clone(), "{}".into()),
        (fresh.clone(), "x = 1".into()),
    ])
    .unwrap();
    for p in [&path, &fresh] {
        let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", p.display());
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
}

/// **A row's `provider` must be one the user has** — a built-in, or one in their own
/// `providers.toml` — checked at load, so a tree naming a provider nobody defined refuses every
/// spawn by name instead of failing only the node that reaches for it. The registry is read
/// only when some row names a provider.
#[test]
fn a_rows_provider_must_be_in_the_users_registry() {
    let parse = |p: &str| {
        marion_core::agent_type::AgentTypes::parse(&format!(
            "[[agent]]\nname = \"x\"\nharness = \"codex\"\ndescription = \"d\"\n{p}"
        ))
        .unwrap()
    };
    let seed = || Ok(marion_core::provider::Registry::seed());
    assert!(check_providers(&parse("provider = \"openrouter\"\n"), seed).is_ok());
    let err = check_providers(&parse("provider = \"nope\"\n"), seed).unwrap_err();
    assert!(
        err.contains("`nope`") && err.contains("marion key list"),
        "{err}"
    );
    let custom = || {
        marion_core::provider::Registry::with_custom(
            "[providers.mine]\nbase_url = \"https://x/v1\"\nwires = [\"openai-chat\"]\n",
        )
        .map_err(|e| e.to_string())
    };
    assert!(check_providers(&parse("provider = \"mine\"\n"), custom).is_ok());
    // No row names a provider: the registry is never consulted, so a broken file cannot
    // refuse a tree that does not use it.
    let unread = || -> Result<marion_core::provider::Registry, String> {
        panic!("the registry was read for a tree that names no provider")
    };
    assert!(check_providers(&parse(""), unread).is_ok());
}

/// A type the tree's file does not define is refused before any side effect, exactly as an
/// unknown built-in is — and the refusal is the same variant, so callers keep one arm.
#[test]
fn a_type_the_file_no_longer_defines_is_refused_before_anything_is_journaled() {
    let (_root, state, repo, env) = spawn_env("agent-types-unknown");
    let mut req = request("reviewer", None);
    req.repo = repo;
    let err = run_spawn(
        &env,
        &req,
        &TaskId("unknown".into()),
        &Caller::root("root", builtin("claude").unwrap()),
    )
    .expect_err("no file defines `reviewer`");
    assert!(
        matches!(&err, SpawnError::UnknownAgentType(t) if t == "reviewer"),
        "{err:?}"
    );
    let mut written = Vec::new();
    files_under(&state, &mut written);
    assert!(
        written.is_empty(),
        "nothing journaled, nothing started: {written:?}"
    );
}

/// A type's `prompt_prefix` goes in front of the prompt exactly once, and a type with none
/// leaves the prompt untouched — byte for byte, so a built-in's node is launched from the
/// bytes it was launched from before the field existed.
#[test]
fn a_user_types_prompt_prefix_is_prepended_once() {
    let plain = builtin("codex-impl").unwrap();
    assert_eq!(prefixed_prompt(&plain, "do the task"), "do the task");
    let reviewer = marion_core::agent_type::AgentTypes::parse(
        "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"r\"\n\
         prompt_prefix = \"Review only.\\n\\n\"\n",
    )
    .unwrap()
    .resolve("reviewer")
    .unwrap();
    let once = prefixed_prompt(&reviewer, "do the task");
    assert_eq!(once, "Review only.\n\ndo the task");
    assert_eq!(once.matches("Review only.").count(), 1);
}

/// **Every child is told, in its prompt, to call `report`.** Measured live (2026-09-22): ten
/// claude and codex children did the work and never called `report`, because the tool's own
/// description was the only place marion said so. The instruction comes last — after a type's
/// prefix and the task — exactly once, and it names the tool by marion's server rather than
/// in any one harness's spelling.
#[test]
fn a_childs_prompt_ends_with_marions_one_report_instruction() {
    let plain = builtin("codex-impl").unwrap();
    let prompt = child_prompt(&plain, "do the task");
    assert_eq!(
        prompt,
        format!("do the task\n\n{}", crate::bridge::REPORT_INSTRUCTION)
    );
    let reviewer = marion_core::agent_type::AgentTypes::parse(
        "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"r\"\n\
         prompt_prefix = \"Review only.\\n\\n\"\n",
    )
    .unwrap()
    .resolve("reviewer")
    .unwrap();
    let prefixed = child_prompt(&reviewer, "do the task");
    assert!(
        prefixed.starts_with("Review only.\n\ndo the task\n\n"),
        "{prefixed}"
    );
    assert_eq!(
        prefixed.matches(crate::bridge::REPORT_INSTRUCTION).count(),
        1,
        "{prefixed}"
    );
    let instruction = crate::bridge::REPORT_INSTRUCTION;
    assert!(
        instruction.contains("marion MCP server") && instruction.contains("`report`"),
        "{instruction}"
    );
    for spelling in ["mcp__marion__report", "marion_report", "marion-report"] {
        assert!(
            !instruction.contains(spelling),
            "harness-neutral: {instruction}"
        );
    }
}

/// **A prefix and a prompt are two sentences, and marion keeps them apart.** A row that ends
/// its prefix on a letter gets one newline between it and the prompt; a row whose author
/// already ended it in whitespace (`"\n\n"`) is joined exactly as written, because that
/// whitespace is the author's own separator and marion must not add a third line to it.
#[test]
fn a_prefix_without_trailing_whitespace_is_separated_from_the_prompt_by_one_newline() {
    let ty = |prefix: &str| {
        marion_core::agent_type::AgentTypes::parse(&format!(
            "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"r\"\n\
             prompt_prefix = {prefix:?}\n",
        ))
        .unwrap()
        .resolve("reviewer")
        .unwrap()
    };
    assert_eq!(
        prefixed_prompt(&ty("You are a reviewer."), "DEMO: review"),
        "You are a reviewer.\nDEMO: review"
    );
    assert_eq!(
        prefixed_prompt(&ty("You are a reviewer.\n\n"), "DEMO: review"),
        "You are a reviewer.\n\nDEMO: review"
    );
    assert_eq!(
        prefixed_prompt(&ty("You are a reviewer. "), "DEMO: review"),
        "You are a reviewer. DEMO: review"
    );
    let plain = builtin("codex-impl").unwrap();
    assert_eq!(prefixed_prompt(&plain, "DEMO: review"), "DEMO: review");
}

/// **A `SpawnAborted` written for a refusal marion can name carries that name.** The guard's
/// generic reason is for the exits it cannot see; a `?` that passed through [`AbortOnDrop::filed`]
/// leaves the error's own sentence in the journal, so a reader of the record learns which tool
/// on which harness rather than only that marion left.
#[test]
fn an_abort_files_the_refusals_own_sentence_when_it_has_one() {
    let (_root, _state, _repo, env) = spawn_env("abort-reason");
    let agent_id = AgentId("abort-1".into());
    let refused: Result<(), SpawnError> = Err(SpawnError::Harness(
        marion_harness::HarnessError::UnsupportedTool {
            harness: Harness::Codex,
            tool: "read".into(),
        },
    ));
    {
        let mut resolution = AbortOnDrop {
            project: &env.project_dir,
            agent_id: agent_id.clone(),
            armed: true,
            reason: None,
        };
        let err = resolution.filed(refused).unwrap_err();
        assert!(
            matches!(err, SpawnError::Harness(_)),
            "the error is returned untouched"
        );
    }
    let bytes = std::fs::read(env.project_dir.journal()).unwrap();
    let replay = marion_core::registry::replay(&bytes);
    let reason = replay
        .get(&agent_id)
        .and_then(|n| n.spawn_aborted.clone())
        .expect("the guard journaled the abort");
    assert!(
        reason.contains("`read`") && reason.contains("codex"),
        "the journal carries the adapter's sentence: {reason}"
    );
}

fn spawn_env(name: &str) -> (Scratch, PathBuf, PathBuf, Env) {
    let root = scratch(&format!("supervisor-{name}"));
    let repo = fixture_repo(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = Env {
        os_sandbox: true,
        project_dir: ProjectDir::new(&state, &repo),
        state: state.clone(),
        project_root: repo.clone(),
        bridge: PathBuf::from("/bin/marion-supervisor"),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        auth: Auth::Canned,
    };
    (root, state, repo, env)
}

/// **The gate, and the side effects it must precede.**
///
/// A caller already at its type's `max_depth` asks for one more level. §3.1: that spawn "is
/// refused with a spawn error, never silently clamped" — so the assertion is threefold, and
/// each part fails against the pre-fix code (which had no gate at all):
///
/// 1. the error is the **typed** `SpawnError::Gate(DepthExceeded { .. })`, not a flattened
///    string and not some later failure that happens to look like a refusal;
/// 2. its message **names the bound and the value** — a refusal that does not say `4` and `3`
///    tells the caller nothing it can act on;
/// 3. **nothing was created.** Walked, not probed at a guessed path, for the same reason the
///    dispatch regression above walks: asserting the absence of one path would pass if the code
///    simply wrote it somewhere else. A worktree, a branch, an agent-dir and an OS process are
///    what this gate exists to prevent, so "refused" has to mean none of them happened.
#[test]
fn a_spawn_past_max_depth_is_refused_by_name_and_creates_nothing() {
    let (root, state, repo, env) = spawn_env("depth-gate");
    let caller = Caller {
        agent_id: "caller".into(),
        agent_type: builtin("claude").unwrap(),
        depth: marion_core::agent_type::DEFAULT_MAX_DEPTH,
        live_children: 0,
    };
    // codex-impl: a type whose child would really launch a process, so a missing gate is a real
    // grandchild rather than a failure somewhere else.
    let mut req = request("codex-impl", None);
    req.repo = repo;

    let err = run_spawn(&env, &req, &TaskId("too-deep".into()), &caller)
        .expect_err("a spawn past max_depth must be refused");

    assert!(
        matches!(
            err,
            SpawnError::Gate(marion_core::agent_type::SpawnGateError::DepthExceeded {
                child_depth: 4,
                max_depth: 3
            })
        ),
        "expected a typed depth refusal, got {err}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("max_depth 3") && msg.contains("depth 4"),
        "the refusal must name the bound AND the value that broke it, got: {msg}"
    );

    let mut written = Vec::new();
    files_under(&state, &mut written);
    assert!(
        written.is_empty(),
        "the gate runs before every side effect there is, so a refused spawn leaves no \
         agent-dir, no config and no contract: {written:?}"
    );
    let worktrees = SysCommand::new("git")
        .current_dir(repo_of(&root))
        .args(["worktree", "list"])
        .output()
        .expect("git runs");
    assert_eq!(
        String::from_utf8_lossy(&worktrees.stdout).lines().count(),
        1,
        "a refused spawn must not have created a worktree: {}",
        String::from_utf8_lossy(&worktrees.stdout)
    );
    let branches = SysCommand::new("git")
        .current_dir(repo_of(&root))
        .args(["branch", "--list", "marion/*"])
        .output()
        .expect("git runs");
    assert!(
        String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "nor a branch: {}",
        String::from_utf8_lossy(&branches.stdout)
    );
}

fn repo_of(root: &Path) -> PathBuf {
    root.join("repo")
}

/// The other half, so the gate cannot pass by refusing everything: one level shallower is
/// **allowed through it**. The run then fails for its own reasons — there is no `codex` process
/// worth starting against a base URL nobody serves — but whatever it fails as, it is not the
/// gate, and it got far enough to create the state a refusal never would.
#[test]
fn a_spawn_within_max_depth_is_not_refused_by_the_gate() {
    // `_root` and not `_`: the underscore-prefixed binding still lives to the end of the test,
    // where its `Drop` removes the scratch dir. A bare `_` would drop it here, mid-test.
    let (_root, state, repo, env) = spawn_env("depth-allowed");
    let caller = Caller {
        agent_id: "caller".into(),
        agent_type: builtin("claude").unwrap(),
        // 2 → the child lands at 3, which is exactly `max_depth` and therefore legal.
        depth: marion_core::agent_type::DEFAULT_MAX_DEPTH - 1,
        live_children: 0,
    };
    let mut req = request("codex-impl", None);
    req.repo = repo;
    req.timeout_secs = 1;

    let result = run_spawn(&env, &req, &TaskId("deep-enough".into()), &caller);

    if let Err(e) = &result {
        assert!(
            !matches!(e, SpawnError::Gate(_)),
            "depth 3 is within max_depth 3 and must not be gated: {e}"
        );
    }
    let mut written = Vec::new();
    files_under(&state, &mut written);
    assert!(
        !written.is_empty(),
        "a spawn the gate let through gets an agent-dir and a config, which is precisely what \
         the refused one above must not have"
    );
}

/// The child's own depth is its caller's plus one, and that is what reaches its bridge — the
/// link without which the gate above could never fire on a grandchild, because the grandchild's
/// bridge would have no depth to check.
#[test]
fn a_childs_bridge_is_told_a_depth_one_below_its_callers() {
    // Held, not dropped: see the note in the test above.
    let (_root, state, repo, env) = spawn_env("depth-carried");
    let caller = Caller {
        agent_id: "caller".into(),
        agent_type: builtin("claude").unwrap(),
        depth: 1,
        live_children: 0,
    };
    let mut req = request("codex-impl", None);
    req.repo = repo;
    req.timeout_secs = 1;
    let _ = run_spawn(&env, &req, &TaskId("carry".into()), &caller);

    let mut written = Vec::new();
    files_under(&state, &mut written);
    let config = written
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "config.toml"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_else(|| panic!("the codex child's config was not written: {written:?}"));
    assert!(
        config.contains(r#"MARION_DEPTH = "2""#),
        "a child of a depth-1 caller is at depth 2, and its bridge must be told so:\n{config}"
    );
    assert!(
        config.contains(r#"MARION_AGENT_TYPE = "codex""#),
        "and told its own canonical type, whose max_depth its own spawns are gated on:\n{config}"
    );
}

/// **The concurrency half, now that it can bind.**
///
/// This test used to be called
/// `the_concurrency_gate_is_wired_and_cannot_bind_while_spawn_is_synchronous` and its doc said
/// *"when backgrounding lands, the count changes and this test is the one that should start
/// failing"*. It is that test, rewritten rather than deleted, because the fact it pinned has
/// not gone away — it has inverted, and the inversion is the whole point of the change.
///
/// What it pins now: the gate reads a *field*, so the caller's count is whatever the bridge
/// measured, and the bound refuses at exactly `max_concurrent_children` on every built-in —
/// refuses, never queues (§3.1). The end-to-end witness that a second **backgrounded** spawn
/// is refused is `tests/it_canned/background_spawn.rs`; this is the unit that would catch an
/// off-by-one in the bound itself, which no end-to-end test could localize.
#[test]
fn the_concurrency_gate_refuses_at_the_bound_and_admits_below_it() {
    for name in marion_core::agent_type::builtin_names() {
        let t = builtin(name).unwrap();
        let max = t.max_concurrent_children;
        assert!(
            check_spawn_gates(&t, 0, max - 1).is_ok(),
            "{name}: a caller one below its bound may still spawn"
        );
        assert!(
            check_spawn_gates(&t, 0, max).is_err(),
            "{name}: at the bound the spawn is refused, not queued (§3.1)"
        );
        assert!(
            check_spawn_gates(&t, 0, max + 1).is_err(),
            "{name}: and above it too — the gate is `>=`, so a count that somehow overshot is \
             still refused rather than wrapping back to admitted"
        );
    }
}

/// `Caller::root` answers the live count with a measurement, not with the old constant.
///
/// Separate from the gate test above because it guards a different mistake: re-introducing a
/// hardcoded zero by giving the field a default that no bridge ever overwrites. A root that
/// `marion run` just minted genuinely has no children — that is why 0 is right here — and the
/// bridge overwrites it on every `spawn` it serves.
#[test]
fn a_freshly_minted_root_has_no_live_children() {
    let c = Caller::root("root", builtin("claude").unwrap());
    assert_eq!(c.live_children, 0);
    assert_eq!(c.depth, crate::depth::ROOT_DEPTH);
}

#[test]
fn a_requested_scope_outside_the_agent_type_ceiling_is_rejected_before_launch() {
    let ceiling = vec![Glob("src/**".into())];
    let requested = vec![Glob("docs/**".into())];
    assert!(check_spawn_scope(&ceiling, &requested).is_err());
}

// -----------------------------------------------------------------------------------------
// §5.4's `verification`: shell lines run in the child's workspace at its terminal transition.
// -----------------------------------------------------------------------------------------

fn one_command(line: &str, cwd: &Path, timeout: StdDuration) -> Command {
    Command {
        program: "sh".into(),
        args: vec!["-c".into(), line.into()],
        cwd: cwd.to_path_buf(),
        timeout: Duration::from_secs(timeout.as_secs()),
    }
}

#[test]
fn a_passing_verification_command_records_exit_zero_and_its_stdout() {
    let scratch = scratch("verif-pass");
    let cmds = verification_commands(&["echo verified".into()], &scratch, None);
    let out = run_verification(&cmds);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].command, cmds[0],
        "the outcome names the command that produced it"
    );
    assert_eq!(out[0].exit_code, Some(0));
    assert_eq!(out[0].stdout.value, "verified\n");
    assert_eq!(out[0].stderr.value, "");
    assert!(!out[0].timed_out);
}

#[test]
fn a_failing_verification_command_records_its_exit_code_and_stderr() {
    let scratch = scratch("verif-fail");
    let cmds = verification_commands(&["echo broken 1>&2; exit 3".into()], &scratch, None);
    let out = run_verification(&cmds);
    assert_eq!(out[0].exit_code, Some(3));
    assert_eq!(out[0].stderr.value, "broken\n");
    assert_eq!(out[0].stdout.value, "");
    assert!(!out[0].timed_out);
}

/// The bound is the `Command`'s own, and expiry kills the whole group: the `sleep` is `sh`'s
/// child, not `sh` itself, so a kill that reached only the direct child would leave it.
#[test]
fn a_verification_command_over_its_bound_is_killed_and_marked_timed_out() {
    let scratch = scratch("verif-timeout");
    // A fractional sleep no other test in this process runs, so the sweep below finds only
    // this sleeper — `pgrep -f` matches `sh -c`'s argv and the `sleep` it forked alike.
    let marker = format!("sleep 30.{}", std::process::id());
    let cmd = one_command(&marker, &scratch, StdDuration::from_secs(1));
    let started = Instant::now();
    let out = run_verification(std::slice::from_ref(&cmd));
    assert!(out[0].timed_out, "the bound expired");
    assert!(
        started.elapsed() < StdDuration::from_secs(10),
        "the bound is the command's 1 s, not §6.7's 300 s default"
    );
    let deadline = Instant::now() + StdDuration::from_secs(3);
    loop {
        let survivors = SysCommand::new("pgrep")
            .args(["-f", &marker])
            .output()
            .expect("pgrep runs");
        if survivors.stdout.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the sleeper and its shell survived the kill: pids {}",
            String::from_utf8_lossy(&survivors.stdout)
        );
        thread::sleep(StdDuration::from_millis(50));
    }
}

/// Sequential and in the parent's order, each in the workspace it was given: a later command
/// sees what an earlier one wrote, which is what lets `cargo build` precede `cargo test`.
/// **A verification process inherits none of marion's variables or provider keys**: every
/// such name in the supervisor's environment is removed from the process, and nothing else is.
#[test]
fn a_verification_process_withholds_marions_variables_and_provider_keys() {
    for key in [
        "MARION_NODE_TOKEN",
        "MARION_STATE_DIR",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ] {
        assert!(is_withheld_from_verification(key), "{key}");
    }
    for key in ["PATH", "HOME", "CARGO_HOME", "LANG", "MARIONETTE"] {
        assert!(!is_withheld_from_verification(key), "{key}");
    }
    let cmd = verification_commands(&["true".into()], Path::new("/"), None).remove(0);
    let sys = verification_process(&cmd);
    let removed: Vec<String> = sys
        .get_envs()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    for (key, _) in std::env::vars() {
        assert_eq!(
            removed.contains(&key),
            is_withheld_from_verification(&key),
            "{key}"
        );
    }
}

/// **A codex child's verification runs inside codex's own workspace sandbox**: a line may write
/// in the worktree and is refused outside it (codex leaves the temp dir writable as well). Needs a real `codex`, carrying its row's update
/// switch; skipped by name where there is none.
#[test]
fn a_sandboxed_childs_verification_cannot_write_outside_its_workspace() {
    if !marion_testsupport::harness_available("codex") {
        return;
    }
    let dir = marion_testsupport::scratch("run-verify-sandbox");
    let wt = dir.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    // Not under the temp dir, which codex's `workspace-write` leaves writable too: a path in
    // this crate's own directory, removed afterwards whatever happens.
    let outside = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("verify-sandbox-probe-{}", std::process::id()));
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _gone = Gone(outside.clone());
    let codex = marion_core::agent_type::builtin("codex").unwrap();
    let sandbox = marion_harness::containment::verify_prefix(&codex).expect("codex has one");
    let line = format!(
        "echo in > inside.txt; echo out > '{}'; true",
        outside.display()
    );
    let cmds = verification_commands(&[line], &wt, Some(&sandbox));
    assert_eq!(cmds[0].program, "codex");
    let out = run_verification(&cmds);
    assert_eq!(out[0].exit_code, Some(0), "{out:?}");
    assert!(wt.join("inside.txt").exists(), "the workspace is writable");
    assert!(
        !outside.exists(),
        "a write outside the workspace was refused"
    );
}

#[test]
fn verification_commands_run_in_the_workspace_in_the_parents_order() {
    let scratch = scratch("verif-order");
    let lines = vec![
        "echo first > order.txt".into(),
        "echo second >> order.txt".into(),
        "cat order.txt".into(),
    ];
    let cmds = verification_commands(&lines, &scratch, None);
    assert_eq!(cmds.len(), 3);
    for (c, line) in cmds.iter().zip(&lines) {
        assert_eq!(c.program, "sh");
        assert_eq!(c.args, vec!["-c".to_string(), line.clone()]);
        assert_eq!(&c.cwd, &*scratch);
        assert_eq!(
            c.timeout,
            Duration::from_secs(VERIFICATION_TIMEOUT.as_secs())
        );
    }
    let out = run_verification(&cmds);
    let codes: Vec<_> = out.iter().map(|o| o.exit_code).collect();
    assert_eq!(codes, vec![Some(0), Some(0), Some(0)]);
    assert_eq!(out[2].stdout.value, "first\nsecond\n");
    assert_eq!(
        std::fs::read_to_string(scratch.join("order.txt")).unwrap(),
        "first\nsecond\n",
        "the commands ran in the workspace, not in the test's cwd"
    );
}

/// The runner records `Capped::whole`: the persisted contract is the uncapped record
/// (`cross_product.rs` asserts it), and `cap_for_return` alone shortens the copy handed back.
#[test]
fn a_large_verification_output_is_persisted_whole_and_capped_only_on_return() {
    let scratch = scratch("verif-large");
    let cmds = verification_commands(
        &["yes 0123456789012345678901234567890123456789 | head -n 2000".into()],
        &scratch,
        None,
    );
    let evidence = run_verification(&cmds);
    let bytes = evidence[0].stdout.value.len();
    assert!(
        bytes > marion_core::cap::EVIDENCE_BUDGET,
        "the fixture must overflow the budget to test anything, got {bytes}"
    );
    assert!(!evidence[0].stdout.truncated);
    assert_eq!(evidence[0].stdout.original_bytes, bytes);

    let contract = build_contract(
        TaskId("t".into()),
        AgentId("r".into()),
        marion_core::Harness::Codex,
        RepoIdentity {
            git_common_dir: None,
            head_branch: None,
        },
        None,
        Workspace::Worktree {
            path: scratch.to_path_buf(),
            branch: "b".into(),
        },
        "do it",
        &[],
        &[Glob("**".into())],
        &[Glob("**".into())],
        Duration::from_secs(900),
        SystemTime(std::time::SystemTime::now()),
        &ChildOutcome {
            narrative: Some("done".into()),
            exit_code: Some(0),
            ..ChildOutcome::default()
        },
        Some(vec![]),
        None,
        cmds.clone(),
        evidence,
    );
    let persisted = contract.completion.as_ref().unwrap();
    assert!(
        !persisted.evidence[0].stdout.truncated,
        "the persisted copy is whole"
    );
    assert_eq!(persisted.evidence[0].stdout.value.len(), bytes);
    let returned = cap_for_return(contract.clone());
    let ev = &returned.completion.unwrap().evidence[0];
    assert!(ev.stdout.truncated, "the returned copy is capped");
    assert!(ev.stdout.value.len() < bytes);
    assert_eq!(ev.stdout.original_bytes, bytes);
}
