//! A project on disk as a supervisor leaves one, for the export's tests: a claude root that
//! delegated to two codex children — one that landed a branch, one refused by its provider — with a
//! journal, contracts and streams at fixed times, and a secret planted in every place the design
//! names as a source a report reads from.

use std::path::PathBuf;
use std::time::Duration;

use marion_core::contract::{
    AgentId, Capped, Command, CommandOutcome, ExitStatus, FailureCause, Glob, Oid, ProcessExit,
    RepoIdentity, TaskId, TokenUsage, Workspace,
};
use marion_core::encoding::{Millis, SystemTime};
use marion_core::harness::Harness;
use marion_core::journal::{
    self, ContractPersisted, Exited, JournalRecord, MessageDelivered, MessageQueued, MessageSource,
    RecordKind, SpawnIntent, Spawned, UsageRecorded,
};
use marion_core::paths::ProjectDir;
use marion_core::secret::Secret;

use crate::credentials::{CredentialError, CredentialStore};
use crate::events::{EventSink, EventWriter};
use crate::spawn::ChildOutcome;

/// The home directory every path in the fixture sits under, as the scrubber is told it.
pub const HOME: &str = "/Users/fixture";
/// The repository, under [`HOME`].
pub const REPO: &str = "/Users/fixture/code/app";
/// The report's `generated_at`.
pub const NOW_MS: u64 = 1_790_003_600_000;
const T0: u64 = 1_790_000_000_000;

pub const ROOT: &str = "019f0000-8ea3-7000-8000-000000000001";
pub const LANDED: &str = "019f0000-1b2c-7000-8000-000000000002";
pub const REFUSED: &str = "019f0000-5d6e-7000-8000-000000000003";

/// The endpoint credential the refused child ran on, and the key the fake store holds for it.
pub const CREDENTIAL: &str = "openrouter:work";
pub const STORE_KEY: &str = "memstore-endpoint-key-7777";
/// A secret-named variable of the exporting process's environment, and its value.
pub const ENV_NAME: &str = "X_API_KEY";
pub const ENV_KEY: &str = "envkey-from-X_API_KEY-0001";
/// The root's own prompt: the operator's words, in a report only with `--include-prompt`.
pub const ROOT_PROMPT: &str = "ROOT-PROMPT-SENTINEL: add rate limiting to the api";

/// Every secret planted, by the source it was planted in. None may reach a report.
pub const SENTINELS: &[(&str, &str)] = &[
    ("child instructions", "ghp_INSTRUCTIONS0123456789abcdefgh"),
    ("acceptance criterion", "AKIAACCEPTANCE000001"),
    ("narrative", "sk-proj-NARRATIVE0123456789abcdef"),
    ("tool args (bearer)", "root-bearer-token-0001"),
    ("tool args (slack)", "xoxb-TOOLARGS-0123456789"),
    ("said line", "github_pat_SAIDLINE0123456789_abcdefghij"),
    ("verify stderr", "glpat-VERIFYSTDERR0123456789"),
    ("full diff (hex)", HEX64),
    ("full diff (pem)", "MIIEpAIBAAKCAQEAfixturePEMbody"),
    ("failure line", "sk-ant-api03-FAILURELINE0123456789"),
    ("X_API_KEY env", ENV_KEY),
    ("credential store key", STORE_KEY),
];

const HEX64: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// The fixture project, removed with its scratch directory when dropped.
pub struct Fixture {
    pub project: ProjectDir,
    pub repo: PathBuf,
    _dir: marion_testsupport::Scratch,
}

/// A credential store holding [`STORE_KEY`] under [`CREDENTIAL`] and nothing else.
pub struct MemStore;

impl CredentialStore for MemStore {
    fn get(&self, provider: &str) -> Result<Option<Secret>, CredentialError> {
        Ok((provider == CREDENTIAL).then(|| Secret::new(STORE_KEY)))
    }
    fn put(&self, _: &str, _: &Secret) -> Result<(), CredentialError> {
        unreachable!("the export never writes a key")
    }
    fn delete(&self, _: &str) -> Result<bool, CredentialError> {
        unreachable!("the export never removes a key")
    }
    fn describe(&self) -> String {
        "memory".into()
    }
}

/// The scrubber a real export builds, over the fixture's store, environment and home.
pub fn scrubber(credentials: &[String]) -> super::scrub::Scrubber {
    let keys = super::keys_behind(credentials, &MemStore).expect("the memory store answers");
    super::scrub::Scrubber::new(
        keys,
        [
            (ENV_NAME.to_string(), ENV_KEY.to_string()),
            ("HOME".to_string(), HOME.to_string()),
        ],
        Some(HOME.to_string()),
    )
}

pub fn now() -> SystemTime {
    SystemTime::from_unix_millis(NOW_MS)
}

fn at(secs: u64) -> SystemTime {
    SystemTime::from_unix_millis(T0 + secs * 1000)
}

fn id(s: &str) -> AgentId {
    AgentId(s.into())
}

pub fn build(tag: &str) -> Fixture {
    let dir = marion_testsupport::scratch(tag);
    let project = ProjectDir::new(&dir, std::path::Path::new(REPO));
    write_journal(&project);
    write_root(&project);
    write_landed(&project);
    write_refused(&project);
    Fixture {
        project,
        repo: PathBuf::from(REPO),
        _dir: dir,
    }
}

fn write_journal(p: &ProjectDir) {
    let intent = |agent: &str, parent: Option<&str>, harness, ty: &str, task: Option<&str>| {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id(agent),
            parent_id: parent.map(id),
            agent_type: ty.into(),
            harness,
            depth: u32::from(parent.is_some()),
            task_id: task.map(|t| TaskId(t.into())),
            timeout_secs: Some(900),
            verification: Vec::new(),
            review_of: None,
            race: None,
            budget: None,
            workflow: None,
        })
    };
    let spawned = |agent: &str, version: &str, model: &str, credential: Option<&str>| {
        RecordKind::Spawned(Spawned {
            agent_id: id(agent),
            harness_version: version.into(),
            model: Some(model.into()),
            pid: None,
            start_id: None,
            provider: credential.map(|_| "openrouter".into()),
            route: credential.map(|_| "native".into()),
            credential: credential.map(str::to_string),
        })
    };
    let exited = |agent: &str, status, description: &str| {
        RecordKind::Exited(Exited {
            agent_id: id(agent),
            status,
            exit: ProcessExit {
                code: Some(i32::from(status != ExitStatus::Ok)),
                signal: None,
                description: description.into(),
            },
        })
    };
    let usage = |agent: &str, input, output, cache_read, turns: Vec<u64>| {
        RecordKind::UsageRecorded(UsageRecorded {
            agent_id: id(agent),
            usage: TokenUsage {
                input,
                output,
                cache_read,
                ..TokenUsage::default()
            },
            turns,
        })
    };
    let persisted = |agent: &str, task: &str, status| {
        RecordKind::ContractPersisted(ContractPersisted {
            agent_id: id(agent),
            task_id: TaskId(task.into()),
            requester: id(ROOT),
            status: Some(status),
            review: None,
            changed: None,
        })
    };
    let records = [
        (0, intent(ROOT, None, Harness::ClaudeCode, "claude", None)),
        (1, spawned(ROOT, "2.1.268", "claude-opus-5-5", None)),
        (
            5,
            intent(LANDED, Some(ROOT), Harness::Codex, "codex", Some("t-1")),
        ),
        (6, spawned(LANDED, "0.155.1", "gpt-5.5-codex", None)),
        (
            20,
            intent(REFUSED, Some(ROOT), Harness::Codex, "codex", Some("t-2")),
        ),
        (21, spawned(REFUSED, "0.155.1", "gpt-5.5", Some(CREDENTIAL))),
        (
            40,
            RecordKind::MessageQueued(MessageQueued {
                agent_id: id(LANDED),
                message_id: "m-1".into(),
                source: MessageSource::Ancestor(id(ROOT)),
                len: 58,
                sha256: "00".into(),
            }),
        ),
        (
            42,
            RecordKind::MessageDelivered(MessageDelivered {
                agent_id: id(LANDED),
                message_id: "m-1".into(),
                via: "turn".into(),
                note: None,
            }),
        ),
        (50, usage(REFUSED, 900, 12, 0, vec![912])),
        (50, persisted(REFUSED, "t-2", ExitStatus::Failed)),
        (
            50,
            exited(
                REFUSED,
                ExitStatus::Failed,
                "exited 1 after a refused login",
            ),
        ),
        (
            200,
            usage(LANDED, 18_000, 2_400, 64_000, vec![20_400, 30_000, 14_000]),
        ),
        (200, persisted(LANDED, "t-1", ExitStatus::Ok)),
        (200, exited(LANDED, ExitStatus::Ok, "exited 0")),
        (
            260,
            usage(ROOT, 42_000, 5_100, 210_000, vec![100_000, 157_100]),
        ),
        (260, exited(ROOT, ExitStatus::Ok, "exited 0")),
    ];
    let mut bytes = Vec::new();
    for (seq, (secs, kind)) in records.into_iter().enumerate() {
        let record = JournalRecord {
            writer: journal::WriterId("w".into()),
            seq: seq as u64,
            ts: at(secs),
            mono_ns: 0,
            provenance: marion_core::ir::Provenance::marion(),
            src_seq: None,
            kind,
        };
        bytes.extend(journal::encode(&record).unwrap());
    }
    std::fs::create_dir_all(p.journal().parent().unwrap()).unwrap();
    std::fs::write(p.journal(), bytes).unwrap();
}

/// `frames` recorded as `agent`'s stream the way its sink records them, then re-stamped one every
/// `step` seconds from `from` so the report's times are fixed.
fn write_stream(
    p: &ProjectDir,
    agent: &str,
    harness: Harness,
    from: u64,
    step: u64,
    frames: &[String],
) {
    let path = p.agent(&id(agent)).events();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    {
        let sink = EventSink::new(
            EventWriter::open_path(&path, &id(agent)).unwrap(),
            harness,
            "unused".into(),
        );
        sink.lifecycle(marion_core::event::Lifecycle::Opened);
        for f in frames {
            sink.record_line(f);
        }
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
        v["ts"] = serde_json::to_value(at(from + i as u64 * step)).unwrap();
        out.push_str(&serde_json::to_string(&v).unwrap());
        out.push('\n');
    }
    std::fs::write(&path, out).unwrap();
}

fn claude_call(name: &str, input: serde_json::Value) -> String {
    serde_json::json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": format!("tu-{name}-{input}"), "name": name, "input": input}
    ]}})
    .to_string()
}

fn claude_text(text: &str) -> String {
    serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
        .to_string()
}

fn write_root(p: &ProjectDir) {
    crate::node_detail::persist_root_prompt(&p.agent(&id(ROOT)), ROOT_PROMPT);
    let frames = [
        claude_text("I'll read the api first, then hand the limiter to codex."),
        claude_call(
            "Read",
            serde_json::json!({"file_path": format!("{REPO}/src/lib.rs")}),
        ),
        claude_call(
            "Read",
            serde_json::json!({"file_path": format!("{REPO}/src/api.rs")}),
        ),
        claude_call(
            "Read",
            serde_json::json!({"file_path": format!("{REPO}/src/limits.rs")}),
        ),
        claude_call(
            "mcp__marion__spawn",
            serde_json::json!({"agent_type": "codex", "prompt": "add a token bucket to src/limits"}),
        ),
        claude_call(
            "Bash",
            serde_json::json!({"command": format!(
                "curl -s -H 'Authorization: Bearer {}' https://api.example.invalid/health",
                SENTINELS[3].1
            )}),
        ),
        claude_call(
            "Bash",
            serde_json::json!({"command": format!("echo {} > /dev/null", SENTINELS[4].1)}),
        ),
        claude_text("The child landed marion/t-1; merge it with git merge --no-ff marion/t-1."),
    ];
    write_stream(p, ROOT, Harness::ClaudeCode, 2, 4, &frames);
}

/// A codex item as app-server reports its end (S36 P4): `item/completed` with the item in `params`.
fn codex_item(item: serde_json::Value) -> String {
    serde_json::json!({"method": "item/completed", "params": {"item": item}}).to_string()
}

fn write_landed(p: &ProjectDir) {
    let command = |n: u32, cmd: &str| {
        codex_item(serde_json::json!({
            "id": format!("item_{n}"), "type": "commandExecution", "command": cmd,
            "aggregatedOutput": "", "exitCode": 0, "status": "completed"
        }))
    };
    let frames = [
        command(0, "bash -lc 'rg -n limiter src'"),
        command(1, "bash -lc 'rg -n bucket src'"),
        command(2, "bash -lc 'rg -n refill src'"),
        codex_item(serde_json::json!({
            "id": "item_3", "type": "fileChange", "status": "completed",
            "changes": [{"path": "src/limits/bucket.rs", "kind": "add"},
                        {"path": "src/limits/mod.rs", "kind": "update"}]
        })),
        command(
            4,
            &format!("bash -lc 'curl -H \"x-api-key: {ENV_KEY}\" localhost:1'"),
        ),
        command(5, &format!("OPENROUTER_KEY={STORE_KEY} ./probe")),
        command(6, "bash -lc 'cargo test -q'"),
        codex_item(serde_json::json!({
            "id": "item_7", "type": "mcpToolCall", "server": "marion", "tool": "report",
            "arguments": {"narrative": "token bucket added; tests pass"},
            "result": null, "error": null, "status": "completed"
        })),
        codex_item(serde_json::json!({
            "id": "item_8", "type": "agentMessage",
            "text": format!("Done. (debug: {})", SENTINELS[5].1)
        })),
    ];
    write_stream(p, LANDED, Harness::Codex, 8, 20, &frames);

    let mut c = contract(
        "t-1",
        &format!("add a token bucket to src/limits; use {}", SENTINELS[0].1),
    );
    c.acceptance_criteria = vec![Capped::whole(format!(
        "cargo test passes (not {})",
        SENTINELS[1].1
    ))];
    let done = c.completion.as_mut().unwrap();
    done.status = ExitStatus::Ok;
    done.narrative = Some(Capped::whole(format!(
        "Added src/limits/bucket.rs with a refill-on-read token bucket. {}",
        SENTINELS[2].1
    )));
    done.changed_paths = vec!["src/limits/bucket.rs".into(), "src/limits/mod.rs".into()];
    done.branch = Some("marion/t-1".into());
    done.commit = Some(Oid("4f2a9c1e0b7d3a5f6e8c9d0a1b2c3d4e5f6a7b8c".into()));
    done.diff = Some(Capped::whole(format!(
        "+++ b/src/limits/bucket.rs\n+const SEED: &str = \"{HEX64}\";\n\
         +// -----BEGIN RSA PRIVATE KEY-----\n+// {}\n+// -----END RSA PRIVATE KEY-----\n",
        SENTINELS[8].1
    )));
    done.evidence = vec![
        outcome("cargo", &["test", "-q"], Some(0), "", 41_200),
        outcome(
            "cargo",
            &["clippy", "--", "-D", "warnings"],
            Some(101),
            &format!(
                "warning: unused\nerror: token {} in {REPO}/src/x.rs",
                SENTINELS[6].1
            ),
            9_800,
        ),
    ];
    write_contract(p, LANDED, &c);
}

fn write_refused(p: &ProjectDir) {
    let mut c = contract("t-2", "benchmark the limiter against the old one");
    c.child.provider = Some("openrouter".into());
    c.child.credential = Some(CREDENTIAL.into());
    let done = c.completion.as_mut().unwrap();
    done.status = ExitStatus::Failed;
    done.failure_cause = Some(FailureCause::Auth {
        line: format!("401 Unauthorized: key {} was refused", SENTINELS[9].1),
    });
    write_contract(p, REFUSED, &c);
}

fn contract(task: &str, instructions: &str) -> marion_core::contract::TaskContract {
    crate::spawn::build_contract(
        TaskId(task.into()),
        id(ROOT),
        Harness::Codex,
        RepoIdentity {
            git_common_dir: None,
            head_branch: None,
        },
        None::<Oid>,
        Workspace::Worktree {
            path: format!("{REPO}/.marion/worktrees/{task}").into(),
            branch: format!("marion/{task}"),
        },
        &format!("{instructions}\n\n{}", crate::bridge::REPORT_INSTRUCTION),
        &[],
        &[Glob("**".into())],
        &[Glob("**".into())],
        marion_core::encoding::Duration(Duration::from_secs(900)),
        at(6),
        &ChildOutcome {
            exit_code: Some(0),
            ..ChildOutcome::default()
        },
        None,
        None,
        vec![],
        vec![],
    )
}

fn outcome(
    program: &str,
    args: &[&str],
    exit: Option<i32>,
    stderr: &str,
    ms: u64,
) -> CommandOutcome {
    CommandOutcome {
        command: Command {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: format!("{REPO}/.marion/worktrees/t-1").into(),
            timeout: marion_core::encoding::Duration(Duration::from_secs(300)),
        },
        exit_code: exit,
        stdout: Capped::whole(""),
        stderr: Capped::whole(stderr),
        duration: Millis(Duration::from_millis(ms)),
        timed_out: false,
    }
}

fn write_contract(p: &ProjectDir, agent: &str, c: &marion_core::contract::TaskContract) {
    let path = p.agent(&id(agent)).contract(&c.task_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(c).unwrap()).unwrap();
}
