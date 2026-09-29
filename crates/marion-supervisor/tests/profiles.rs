//! **Profiles, end to end, on fake harnesses** — which of the operator's own logins a child runs on,
//! and what marion does (and never does) when a run on one of them fails.
//!
//! A real detached supervisor under live auth, a fake root that holds, and children of user agent
//! types that name profiles. The fakes stand in for `claude` and `codex`: they record their argv
//! and the variables a profile sets or clears, answer the harness's own status probe from a marker
//! file in the profile directory, and act out a clean run, a usage limit, or a refused login from a
//! mode file beside it. **No real login is touched**: `HOME`, the XDG roots and every profile
//! directory are under the test's scratch directory, and the supervisor's environment is cleared
//! down to them.
//!
//! What is pinned:
//! 1. a profiled child gets the profile directory **exactly as stored**, and the secure-store
//!    override is removed from its environment;
//! 2. a child that names no profile runs with its environment untouched;
//! 3. a usage limit with a second profile listed is a notice on the contract and a reading for
//!    `profile list` — **no relaunch, no failover record**;
//! 4. a refused login fails over, journaled, to the next listed profile;
//! 5. a resume continues on the profile the session was recorded under;
//! 6. codex: `CODEX_HOME`, and its usage-limit error is a notice, never a failover.
//!
//! ```sh
//! cargo test -p marion-supervisor --test profiles
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use marion_core::contract::{AgentId, FailureCause, Isolation, TaskContract};
use marion_core::journal::{ProfileFailover, RecordKind};
use marion_core::proto::params::{AgentSpawnParams, NodeResumeParams, SpawnCaller};
use marion_core::proto::{Call, Frame, Method, MethodResult, Outcome, Request, RequestId};
use marion_supervisor::socket::{SocketPaths, own_uid, project_root, socket_paths};
use marion_testsupport::{Scratch, fixture_repo, git, scratch, write_executable};

mod common;
use common::{shell_quote, walk};

/// A bound that exists only to fail.
const BOUND: Duration = Duration::from_secs(60);

/// In the root's argv and nowhere else, so the fake codex can tell the root that holds from a
/// child that acts.
const ROOT_MARKER: &str = "MARION-PROFILES-ROOT-7c1e";

const AGENTS_TOML: &str = r#"
[[agent]]
name = "two-accounts"
harness = "claude-code"
description = "A claude child on the work login, failing over to personal on a refused login."
profile = ["work", "personal"]

[[agent]]
name = "plain"
harness = "claude-code"
description = "A claude child that names no profile."

[[agent]]
name = "codex-accounts"
harness = "codex"
description = "A codex child on two logins."
profile = ["cwork", "cpersonal"]
"#;

/// A `claude` that records every invocation on one line — `claude|<CLAUDE_CONFIG_DIR>|<secure
/// store override set?>|<argv>` — answers `auth status --json` from `.fake-login`, and otherwise
/// speaks just enough stream-json to be a duplex child: it touches the bridge's ready file named
/// in its `--mcp-config`, answers `initialize`, reads the prompt, names a session after its
/// profile directory, and ends as `.fake-mode` says.
fn fake_claude(bin: &Path, log: &Path) {
    let script = format!(
        r#"#!/bin/sh
secure=unset
[ "${{CLAUDE_SECURESTORAGE_CONFIG_DIR+x}}" = x ] && secure=set
printf 'claude|%s|%s|%s\n' "${{CLAUDE_CONFIG_DIR-<unset>}}" "$secure" "$*" >> {log}
case "$1" in --version) echo "2.1.283 (Claude Code)"; exit 0 ;; esac
if [ "$1" = auth ] && [ "$2" = status ]; then
  if [ -e "$CLAUDE_CONFIG_DIR/.fake-login" ]; then echo '{{"loggedIn": true}}'; exit 0; fi
  echo '{{"loggedIn": false}}'; exit 1
fi
mode=ok
[ -f "${{CLAUDE_CONFIG_DIR:-/nonexistent}}/.fake-mode" ] && mode=$(cat "$CLAUDE_CONFIG_DIR/.fake-mode")
cfg=; prev=
for a in "$@"; do [ "$prev" = --mcp-config ] && cfg=$a; prev=$a; done
ready=$(tr -d '\n' < "$cfg" | sed -n 's/.*"MARION_READY_FILE"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
# Whether marion handed this process a readiness marker some earlier process had already touched:
# a stale one would let the prompt go out before this process's own bridge is up.
if [ -e "$ready" ]; then echo stale >> {log}.ready; else echo fresh >> {log}.ready; fi
[ -n "$ready" ] && : > "$ready"
IFS= read -r init
id=$(printf '%s' "$init" | sed -n 's/.*"request_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"%s","response":{{}}}}}}\n' "$id"
IFS= read -r user
printf '{{"type":"system","subtype":"init","session_id":"sess-%s"}}\n' "$(basename "${{CLAUDE_CONFIG_DIR:-none}}")"
case "$mode" in
  limit)
    printf '%s\n' '{{"type":"rate_limit_event","rate_limit_info":{{"status":"rejected","resetsAt":1790538000,"rateLimitType":"five_hour"}}}}'
    printf '%s\n' '{{"type":"result","subtype":"success","is_error":true,"result":"You'"'"'ve hit your session limit · resets 7:50pm"}}'
    exit 1 ;;
  auth)
    printf '%s\n' '{{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Please run /login"}}'
    exit 1 ;;
  *)
    printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"done"}}'
    exit 0 ;;
esac
"#,
        log = shell_quote(log),
    );
    write_executable(&bin.join("claude"), &script);
}

/// A `codex` (the app-server shim, `common::app_server`) that holds when its turn is the root's and
/// otherwise records itself — `codex|<CODEX_HOME>|<argv>` — answers `login status` from
/// `.fake-login`, and ends its turn as `.fake-mode` says.
fn fake_codex(bin: &Path, log: &Path, gate: &Path, root_argv: &Path) {
    let script = format!(
        r#"#!/bin/sh
case "$1" in --version) echo "codex-cli 0.155.1"; exit 0 ;; esac
if [ -n "$MARION_SHIM_TURN" ]; then
  case "$1" in
    *{root}*)
      printf '%s TOKEN_FROM_ENV="%s"\n' "$MARION_SHIM_ARGV" "${{MARION_NODE_TOKEN-}}" > {root_argv}
      waited=0
      while [ ! -e {gate} ]; do
        sleep 0.05; waited=$((waited + 1)); [ "$waited" -gt 2400 ] && exit 0
      done
      exit 0 ;;
  esac
  printf 'codex|%s|%s\n' "${{CODEX_HOME-<unset>}}" "$MARION_SHIM_ARGV" >> {log}
  mode=ok
  [ -f "${{CODEX_HOME:-/nonexistent}}/.fake-mode" ] && mode=$(cat "$CODEX_HOME/.fake-mode")
  case "$mode" in
    limit)
      # app-server's `error` notification (conformance P-errors' shape), in codex's words.
      printf '%s\n' '{{"method":"error","params":{{"error":{{"message":"You'"'"'ve hit your usage limit.","codexErrorInfo":"usageLimitExceeded"}},"willRetry":false}}}}'
      exit 1 ;;
    *) exit 0 ;;
  esac
fi
printf 'codex|%s|%s\n' "${{CODEX_HOME-<unset>}}" "$*" >> {log}
# The status probe carries the row's update switch ahead of its own argv, as codex takes it.
[ "$1" = -c ] && shift 2
if [ "$1" = login ] && [ "$2" = status ]; then
  if [ -e "$CODEX_HOME/.fake-login" ]; then echo "Logged in using ChatGPT"; exit 0; fi
  echo "Not logged in"; exit 1
fi
exit 0
"#,
        root = ROOT_MARKER,
        gate = shell_quote(gate),
        log = shell_quote(log),
        root_argv = shell_quote(root_argv),
    );
    common::app_server::fake_codex(bin, &script);
}

struct Bed {
    _scratch: Scratch,
    repo: PathBuf,
    state: PathBuf,
    log: PathBuf,
    gate: PathBuf,
    config: PathBuf,
    data: PathBuf,
    supervisor: Child,
    paths: SocketPaths,
    root: AgentId,
    token: String,
}

impl Drop for Bed {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gate, "");
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
    }
}

/// A profile directory under the bed's data root, with its mode.
fn profile_dir(data: &Path, harness: &str, name: &str, mode: &str) -> PathBuf {
    let dir = data.join("marion/profiles").join(harness).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".fake-mode"), mode).unwrap();
    dir
}

/// The bed, with `work`'s claude mode and `cwork`'s codex mode chosen by the test.
fn bed(tag: &str, work_mode: &str, cwork_mode: &str) -> Bed {
    let s = scratch(tag);
    let dir = s.to_path_buf();
    let repo = fixture_repo(&dir);
    let file = repo.join(".marion/agents.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, AGENTS_TOML).unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=marion@example.invalid",
            "-c",
            "user.name=marion",
            "commit",
            "-qm",
            "agent types",
        ],
    );
    let (state, bin, home, config, data) = (
        dir.join("state"),
        dir.join("bin"),
        dir.join("home"),
        dir.join("config"),
        dir.join("data"),
    );
    for d in [&state, &bin, &home, &config] {
        std::fs::create_dir_all(d).unwrap();
    }
    let log = dir.join("invocations.log");
    let gate = dir.join("gate");
    fake_claude(&bin, &log);
    let root_argv = dir.join("root-argv");
    fake_codex(&bin, &log, &gate, &root_argv);
    let work = profile_dir(&data, "claude-code", "work", work_mode);
    let personal = profile_dir(&data, "claude-code", "personal", "ok");
    let cwork = profile_dir(&data, "codex", "cwork", cwork_mode);
    let cpersonal = profile_dir(&data, "codex", "cpersonal", "ok");
    let entry = |name: &str, harness: &str, d: &Path| {
        format!(
            "[[profile]]\nname = \"{name}\"\nharness = \"{harness}\"\ndir = \"{}\"\n\n",
            d.display()
        )
    };
    std::fs::create_dir_all(config.join("marion")).unwrap();
    std::fs::write(
        config.join("marion/profiles.toml"),
        [
            entry("work", "claude-code", &work),
            entry("personal", "claude-code", &personal),
            entry("cwork", "codex", &cwork),
            entry("cpersonal", "codex", &cpersonal),
        ]
        .concat(),
    )
    .unwrap();
    for d in ["/usr/bin", "/bin"] {
        for h in ["claude", "codex"] {
            assert!(!Path::new(d).join(h).exists(), "{d}/{h} could be launched");
        }
    }
    let key = project_root(&repo);
    let paths = socket_paths(&state, &key, own_uid());
    let supervisor = Command::new(env!("CARGO_BIN_EXE_marion-supervisor"))
        .args([
            "serve",
            "--state-dir",
            &state.to_string_lossy(),
            "--project-root",
            &key.to_string_lossy(),
            "--idle-grace-ms",
            "600000",
            "--auth",
            "inherited",
            "--detached",
        ])
        // **Cleared down to the bed**: nothing of the operator's own environment — a real
        // `CLAUDE_CONFIG_DIR`, a real login — may reach a fake, and the one override a profile
        // must remove is planted on purpose.
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", &data)
        .env(
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            dir.join("planted-secure-store"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the supervisor starts");
    let deadline = Instant::now() + BOUND;
    while std::os::unix::net::UnixStream::connect(paths.socket()).is_err() {
        assert!(Instant::now() < deadline, "the supervisor never bound");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut bed = Bed {
        _scratch: s,
        repo,
        state,
        log,
        gate,
        config,
        data,
        supervisor,
        paths,
        root: AgentId(String::new()),
        token: String::new(),
    };
    let root = bed.spawn(AgentSpawnParams {
        review_of: None,
        agent_type: "codex".into(),
        prompt: format!("{ROOT_MARKER}: hold"),
        repo: Some(bed.repo.clone()),
        no_change_record: Some(true),
        timeout_secs: Some(120),
        ..params()
    });
    bed.token = root_token(&root_argv);
    bed.root = root.0;
    bed
}

/// **The root's capability token, off its environment.** A live codex root carries marion's
/// declaration on `-c` pairs rather than in a file (`codex::SPEC`'s live route), and the token
/// rides codex's environment beside it rather than on argv, so the fake root records it from
/// there.
fn root_token(root_argv: &Path) -> String {
    const KEY: &str = "TOKEN_FROM_ENV=\"";
    let deadline = Instant::now() + BOUND;
    loop {
        let argv = std::fs::read_to_string(root_argv).unwrap_or_default();
        if let Some(at) = argv.find(KEY) {
            let rest = &argv[at + KEY.len()..];
            let token = &rest[..rest.find('"').expect("closed")];
            assert!(!token.is_empty(), "the root's environment carries no token");
            return token.to_string();
        }
        assert!(Instant::now() < deadline, "the root never started: {argv}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn params() -> AgentSpawnParams {
    AgentSpawnParams {
        review_of: None,
        agent_type: String::new(),
        prompt: String::new(),
        native_launch: None,
        caller: None,
        notify_parent: false,
        repo: None,
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec![],
        timeout_secs: Some(60),
        model: None,
        no_change_record: None,
        pane: None,
        isolation: None,
        allow_concurrent_writes: None,
        profile: None,
        candidates: vec![],
        race: None,
    }
}

impl Bed {
    fn call(&self, call: Call) -> Result<serde_json::Value, String> {
        use std::io::{BufRead, BufReader, Write};
        let mut c = std::os::unix::net::UnixStream::connect(self.paths.socket()).unwrap();
        c.set_read_timeout(Some(BOUND)).unwrap();
        let frame = Frame::Request(Request::new(RequestId::Number(1), call));
        c.write_all(frame.to_line().as_bytes()).unwrap();
        let mut r = BufReader::new(c.try_clone().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                r.read_line(&mut line).unwrap() > 0,
                "closed without answering"
            );
            if let Frame::Response(resp) = Frame::from_line(&line).unwrap() {
                return match resp.outcome {
                    Outcome::Result(v) => Ok(v),
                    Outcome::Error(e) => Err(e.message),
                };
            }
        }
    }

    /// `agent/spawn`, answered at `Spawned`: the node and its task.
    fn spawn(&self, p: AgentSpawnParams) -> (AgentId, Option<String>) {
        let v = self.call(Call::AgentSpawn(p)).expect("spawned");
        let MethodResult::AgentSpawn(r) = Method::AgentSpawn.decode_result(&v).unwrap() else {
            panic!("an agent/spawn result");
        };
        (r.agent_id, r.task_id.map(|t| t.0))
    }

    /// A child of the root, in the root's own checkout, run to its contract.
    fn child(&self, agent_type: &str) -> (AgentId, TaskContract) {
        let (id, task) = self.spawn(AgentSpawnParams {
            review_of: None,
            agent_type: agent_type.into(),
            prompt: "do the task".into(),
            caller: Some(SpawnCaller {
                agent_id: self.root.clone(),
                node_token: self.token.clone().into(),
            }),
            isolation: Some(Isolation::SharedCwd),
            allow_concurrent_writes: Some(true),
            ..params()
        });
        (id, self.contract(&task.expect("a child has a task")))
    }

    fn contract(&self, task: &str) -> TaskContract {
        let deadline = Instant::now() + BOUND;
        loop {
            let mut found = None;
            walk(&self.state, &mut |p| {
                if p.file_name()
                    .is_some_and(|n| n == format!("{task}.json").as_str())
                {
                    found = Some(p.to_path_buf());
                }
            });
            if let Some(c) = found
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| serde_json::from_slice::<TaskContract>(&b).ok())
                .filter(|c| c.completion.is_some())
            {
                return c;
            }
            assert!(Instant::now() < deadline, "no finished contract for {task}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn journal(&self) -> Vec<RecordKind> {
        let mut path = None;
        walk(&self.state, &mut |p| {
            if p.file_name().is_some_and(|n| n == "journal.jsonl") {
                path = Some(p.to_path_buf());
            }
        });
        common::journal::records(&path.expect("a journal"))
    }

    /// The fake's invocations of `program`, **without** the `--version` probe, as
    /// `(dir, secure-store override, argv)`.
    fn invocations(&self, program: &str) -> Vec<(String, String, String)> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(4, '|');
                (f.next()? == program).then_some(())?;
                let dir = f.next()?.to_string();
                let (secure, argv) = match program {
                    "claude" => (f.next()?.to_string(), f.next()?.to_string()),
                    _ => (String::new(), f.next()?.to_string()),
                };
                (argv != "--version").then_some((dir, secure, argv))
            })
            .collect()
    }

    fn dir(&self, harness: &str, name: &str) -> String {
        self.data
            .join("marion/profiles")
            .join(harness)
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
}

fn description(c: &TaskContract) -> &str {
    &c.completion.as_ref().unwrap().exit.description
}

fn cause(c: &TaskContract) -> Option<&FailureCause> {
    c.completion.as_ref().unwrap().failure_cause.as_ref()
}

fn failovers(journal: &[RecordKind]) -> Vec<&ProfileFailover> {
    journal
        .iter()
        .filter_map(|k| match k {
            RecordKind::ProfileFailover(f) => Some(f),
            _ => None,
        })
        .collect()
}

fn session_profiles(journal: &[RecordKind], id: &AgentId) -> Vec<Option<String>> {
    journal
        .iter()
        .filter_map(|k| match k {
            RecordKind::SessionObserved(s) if &s.agent_id == id => Some(s.profile.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_profiled_child_runs_on_its_directory_exactly_and_without_the_secure_store_override() {
    let bed = bed("profiles-exact", "ok", "ok");
    let (id, _) = bed.child("two-accounts");
    let runs = bed.invocations("claude");
    assert_eq!(runs.len(), 1, "one launch: {runs:?}");
    assert_eq!(
        runs[0].0,
        bed.dir("claude-code", "work"),
        "exactly as stored"
    );
    assert_eq!(
        runs[0].1, "unset",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR is removed"
    );
    assert_eq!(
        session_profiles(&bed.journal(), &id),
        [Some("work".to_string())]
    );
    let _ = &bed.config;
}

#[test]
fn a_child_that_names_no_profile_runs_with_its_environment_untouched() {
    let bed = bed("profiles-none", "ok", "ok");
    let (id, _) = bed.child("plain");
    let runs = bed.invocations("claude");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].0, "<unset>", "no CLAUDE_CONFIG_DIR");
    assert_eq!(
        runs[0].1, "set",
        "the operator's own variable is left alone"
    );
    assert_eq!(session_profiles(&bed.journal(), &id), [None]);
}

/// **A usage limit is a notice.** Two profiles are listed, and the second is never touched: one
/// launch, no failover record, the notice leads the contract's description, the cause is on the
/// contract, and the reading is kept for `marion profile list`.
#[test]
fn a_usage_limit_is_a_notice_and_never_a_relaunch_or_a_failover() {
    let bed = bed("profiles-limit", "limit", "ok");
    let (_, contract) = bed.child("two-accounts");
    let runs = bed.invocations("claude");
    assert_eq!(runs.len(), 1, "no relaunch on a limit: {runs:?}");
    assert_eq!(runs[0].0, bed.dir("claude-code", "work"));
    assert!(failovers(&bed.journal()).is_empty(), "no failover recorded");
    assert!(
        description(&contract)
            .starts_with("profile `work` (claude) hit its session limit; resets 7:50pm; "),
        "{}",
        description(&contract)
    );
    assert!(matches!(
        cause(&contract),
        Some(FailureCause::UsageLimit {
            resets_at: Some(1_790_538_000),
            ..
        })
    ));
    assert_ne!(
        contract.completion.as_ref().unwrap().status,
        marion_core::contract::ExitStatus::Ok,
        "not Ok, so the tree flags it for attention"
    );
    let usage = std::fs::read_to_string(bed.state.join("profiles/claude-code/work.json"))
        .expect("the reading is kept");
    assert!(usage.contains("\"rejected\""), "{usage}");
}

/// **A refused login fails over**, journaled, to the next profile the agent type listed, and the
/// relaunch is a fresh session on that profile.
#[test]
fn a_refused_login_fails_over_to_the_next_listed_profile() {
    let bed = bed("profiles-auth", "auth", "ok");
    let (id, contract) = bed.child("two-accounts");
    let ready = std::fs::read_to_string(bed.log.with_extension("log.ready")).unwrap();
    assert_eq!(
        ready.lines().collect::<Vec<_>>(),
        ["fresh", "fresh"],
        "each launch waits on a marker its own bridge writes"
    );
    let runs: Vec<String> = bed.invocations("claude").into_iter().map(|r| r.0).collect();
    assert_eq!(
        runs,
        [
            bed.dir("claude-code", "work"),
            bed.dir("claude-code", "personal")
        ]
    );
    let journal = bed.journal();
    let f = failovers(&journal);
    assert_eq!(f.len(), 1);
    assert_eq!(
        (f[0].from.as_str(), f[0].to.as_str(), f[0].cause.as_str()),
        ("work", "personal", "auth")
    );
    assert_eq!(
        session_profiles(&journal, &id),
        [Some("work".to_string()), Some("personal".to_string())]
    );
    assert!(!matches!(cause(&contract), Some(FailureCause::Auth { .. })));
}

/// **A resume continues on the recorded profile** — even after `[default]` names another one.
#[test]
fn a_resume_keeps_the_profile_its_session_was_recorded_under() {
    let bed = bed("profiles-resume", "auth", "ok");
    let (id, _) = bed.child("two-accounts");
    std::fs::write(
        bed.config.join("marion/profiles.toml"),
        std::fs::read_to_string(bed.config.join("marion/profiles.toml")).unwrap()
            + "[default]\nclaude-code = \"work\"\n",
    )
    .unwrap();
    // A child whose parent still holds it is its parent's to reach, so the root is let go first.
    std::fs::write(&bed.gate, "").unwrap();
    let deadline = Instant::now() + BOUND;
    while !bed
        .journal()
        .iter()
        .any(|k| matches!(k, RecordKind::Exited(e) if e.agent_id == bed.root))
    {
        assert!(Instant::now() < deadline, "the root never exited");
        std::thread::sleep(Duration::from_millis(20));
    }
    bed.call(Call::NodeResume(NodeResumeParams {
        agent_id: id.clone(),
        prompt: "carry on".into(),
    }))
    .expect("the exited child resumes");
    let deadline = Instant::now() + BOUND;
    while bed.invocations("claude").len() < 3 {
        assert!(Instant::now() < deadline, "the resume never launched");
        std::thread::sleep(Duration::from_millis(20));
    }
    let resumed = &bed.invocations("claude")[2];
    assert_eq!(resumed.0, bed.dir("claude-code", "personal"));
    assert!(
        resumed.2.contains("--resume sess-personal"),
        "{}",
        resumed.2
    );
}

#[test]
fn a_codex_child_gets_its_home_and_a_usage_limit_is_only_a_notice() {
    let bed = bed("profiles-codex", "ok", "limit");
    let (_, contract) = bed.child("codex-accounts");
    let runs = bed.invocations("codex");
    assert_eq!(runs.len(), 1, "no relaunch on a limit: {runs:?}");
    assert_eq!(runs[0].0, bed.dir("codex", "cwork"));
    assert!(failovers(&bed.journal()).is_empty());
    // app-server's `error` notification carries no reset instant (S36's schema: `TurnError` is
    // `{message, codexErrorInfo, additionalDetails}`), so none is claimed.
    assert!(
        description(&contract).starts_with("profile `cwork` (codex) hit its usage limit"),
        "{}",
        description(&contract)
    );
    assert!(matches!(
        cause(&contract),
        Some(FailureCause::UsageLimit { .. })
    ));
}

/// **A root runs on the profile its client names**, else its agent type's first; and a node may
/// never choose its children's account — `profile` beside a `caller` is refused by name.
#[test]
fn a_root_runs_on_the_named_profile_and_a_node_may_not_choose_one() {
    let bed = bed("profiles-root", "ok", "ok");
    let root = |profile: Option<&str>| {
        bed.spawn(AgentSpawnParams {
            review_of: None,
            agent_type: "two-accounts".into(),
            prompt: "root".into(),
            repo: Some(bed.repo.clone()),
            no_change_record: Some(true),
            profile: profile.map(str::to_string),
            ..params()
        })
    };
    let wait_for = |n: usize| {
        let deadline = Instant::now() + BOUND;
        while bed.invocations("claude").len() < n {
            assert!(Instant::now() < deadline, "the root never launched");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    root(Some("personal"));
    wait_for(1);
    root(None);
    wait_for(2);
    let dirs: Vec<String> = bed.invocations("claude").into_iter().map(|r| r.0).collect();
    assert!(
        dirs.contains(&bed.dir("claude-code", "personal")),
        "{dirs:?}"
    );
    assert!(dirs.contains(&bed.dir("claude-code", "work")), "{dirs:?}");
    let refused = bed
        .call(Call::AgentSpawn(AgentSpawnParams {
            review_of: None,
            agent_type: "two-accounts".into(),
            prompt: "x".into(),
            caller: Some(SpawnCaller {
                agent_id: bed.root.clone(),
                node_token: bed.token.clone().into(),
            }),
            profile: Some("personal".into()),
            ..params()
        }))
        .expect_err("a node does not choose an account");
    assert!(refused.contains("must not state `profile`"), "{refused}");
}

/// `marion` itself, with the environment cleared down to `vars` — no real home, no real login.
fn marion(vars: &[(&str, &Path)], args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    cmd.args(args).env_clear();
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd.output().expect("marion runs")
}

/// **`profile add` never runs a login, and `profile list` asks the harness's own probe.** The fake
/// `claude` records every invocation: `add` makes none at all, `list` makes only `auth status
/// --json` with the profile's variable set, and no argv anywhere carries `login`. The listed
/// reading is the one a child's stream left, with its age.
#[test]
fn profile_add_runs_no_login_and_profile_list_reads_the_probe_and_the_stored_reading() {
    let s = scratch("profiles-cli");
    let dir = s.to_path_buf();
    let (bin, home, config, data, state) = (
        dir.join("bin"),
        dir.join("home"),
        dir.join("config"),
        dir.join("data"),
        dir.join("state"),
    );
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
    let log = dir.join("invocations.log");
    fake_claude(&bin, &log);
    let path = PathBuf::from(format!("{}:/usr/bin:/bin", bin.display()));
    let vars = [
        ("PATH", path.as_path()),
        ("HOME", home.as_path()),
        ("XDG_CONFIG_HOME", config.as_path()),
        ("XDG_DATA_HOME", data.as_path()),
        ("MARION_STATE_DIR", state.as_path()),
    ];

    let added = marion(&vars, &["profile", "add", "claude", "work"]);
    let stdout = String::from_utf8_lossy(&added.stdout);
    assert!(
        added.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let work = data.join("marion/profiles/claude-code/work");
    assert!(
        stdout.contains(&format!(
            "CLAUDE_CONFIG_DIR='{}' claude auth login",
            work.display()
        )),
        "the login is printed for the operator: {stdout}"
    );
    assert!(
        std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "`profile add` ran the harness"
    );
    assert!(
        work.join("settings.json")
            .symlink_metadata()
            .unwrap()
            .is_symlink()
    );

    let listed = |want: &str| {
        let out = marion(&vars, &["profile", "list"]);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{text}");
        assert!(text.contains(want), "{want:?} not in:\n{text}");
        text
    };
    listed("work (claude) — logged out, never used");

    std::fs::write(work.join(".fake-login"), "").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::create_dir_all(state.join("profiles/claude-code")).unwrap();
    std::fs::write(
        state.join("profiles/claude-code/work.json"),
        format!(
            r#"{{"last_used_ms":{},"limit":{{"observed_ms":{},"status":"rejected","window":"five_hour","resets_at":0}}}}"#,
            now - 7_200_000,
            now - 120_000
        ),
    )
    .unwrap();
    let text = listed("work (claude) — logged in, used 2h ago");
    assert!(
        text.contains(
            "last limit reading: rejected (five_hour), resets 1970-01-01T00:00:00.000Z — seen 2m ago"
        ),
        "{text}"
    );

    let invocations = std::fs::read_to_string(&log).unwrap();
    for line in invocations.lines() {
        assert!(
            line.ends_with("|auth status --json"),
            "only the status probe runs: {line}"
        );
        assert!(
            line.starts_with(&format!("claude|{}|unset|", work.display())),
            "{line}"
        );
        let argv = line.rsplit('|').next().unwrap();
        assert!(
            !argv.split(' ').any(|a| a == "login" || a == "/login"),
            "{line}"
        );
    }
    assert_eq!(invocations.lines().count(), 2, "one probe per list");
}

/// The same on codex, whose probe is `login status` read for `Not logged in`.
#[test]
fn a_codex_profile_is_listed_from_its_login_status_and_add_runs_nothing() {
    let s = scratch("profiles-cli-codex");
    let dir = s.to_path_buf();
    let (bin, home, config, data) = (
        dir.join("bin"),
        dir.join("home"),
        dir.join("config"),
        dir.join("data"),
    );
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let log = dir.join("invocations.log");
    fake_codex(&bin, &log, &dir.join("gate"), &dir.join("root-argv"));
    let path = PathBuf::from(format!("{}:/usr/bin:/bin", bin.display()));
    let vars = [
        ("PATH", path.as_path()),
        ("HOME", home.as_path()),
        ("XDG_CONFIG_HOME", config.as_path()),
        ("XDG_DATA_HOME", data.as_path()),
    ];
    let added = marion(&vars, &["profile", "add", "codex", "cx"]);
    let stdout = String::from_utf8_lossy(&added.stdout);
    let home_dir = data.join("marion/profiles/codex/cx");
    assert!(
        stdout.contains(&format!("CODEX_HOME='{}' codex login", home_dir.display())),
        "{stdout}"
    );
    assert!(std::fs::read_to_string(&log).unwrap_or_default().is_empty());
    let list = || String::from_utf8_lossy(&marion(&vars, &["profile", "list"]).stdout).into_owned();
    assert!(list().contains("cx (codex) — logged out"), "{}", list());
    std::fs::write(home_dir.join(".fake-login"), "").unwrap();
    assert!(list().contains("cx (codex) — logged in"), "{}", list());
    for line in std::fs::read_to_string(&log).unwrap().lines() {
        assert_eq!(
            line,
            format!(
                "codex|{}|-c check_for_update_on_startup=false login status",
                home_dir.display()
            ),
            "only the status probe runs, carrying the row's update switch"
        );
    }
}
