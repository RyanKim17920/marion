//! **A live node's capability token is never on argv, and the node still reaches marion.**
//!
//! Three rows declare marion's bridge on the harness's own command line on the operator's own
//! login: a live codex node (`-c mcp_servers.marion.…`), a live qwen node (`--mcp-config <json>`)
//! and `copilot --acp` (`--additional-mcp-config <json>`). argv is readable by every user on the
//! machine through `ps`, so each row's token carrier withholds `MARION_NODE_TOKEN` from that
//! declaration. Nor is it set on the harness's environment, which every shell command its model
//! runs inherits: it is written to a 0600 file in the node's own directory, and the harness's
//! environment names that file (`MARION_NODE_TOKEN_FILE`), which the harness passes on to the
//! bridge it starts (`marion_harness::spec::TokenCarrier`).
//!
//! Each test spawns the real harness through `run_spawn` under `Auth::Inherited`, holds the
//! provider's first turn — so the harness and the bridge it started are both alive — and reads the
//! process table while it is held: the token is on no process's argv and in neither the harness's
//! environment nor the bridge's; both name the file, which holds the token, 0600. Released, the
//! node calls marion's `report` — the bridge proved itself with the file's token — which the
//! contract shows.
//!
//! "The operator's own login" is a scratch `HOME` whose harness configuration points at marion's
//! canned provider, so nothing here reaches a vendor or starts a login. The process environment is
//! set once, before any test in this binary starts a process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use marion_core::contract::{AgentId, Isolation, TaskContract, TaskId};
use marion_core::paths::ProjectDir;
use marion_core::secret::Secret;
use marion_provider::{CannedServer, Config, Script, TurnGate};
use marion_supervisor::run::{Caller, Env, SpawnObserver, SpawnRequest, run_spawn_watched};
use marion_testsupport::{Scratch, fixture_repo, harness_available, scratch, sweep, until_within};
use serde_json::json;

const CHILD_TIMEOUT_SECS: u64 = 120;
const NARRATIVE: &str = "Reported through a bridge whose token never touched argv.";
const TOKEN_ENV: &str = marion_harness::mcp_bridge::NODE_TOKEN_ENV;
const TOKEN_FILE_ENV: &str = marion_harness::mcp_bridge::NODE_TOKEN_FILE_ENV;

/// One canned provider per harness, each on its own port so its gate counts one node's turns.
struct Providers {
    codex: Provider,
    qwen: Provider,
    copilot: Provider,
    /// The scratch `HOME` every harness reads its "operator" configuration from. Held for the
    /// binary's life.
    _home: Scratch,
}

struct Provider {
    gate: Arc<TurnGate>,
    _server: CannedServer,
}

fn provider(wire: &str, reqlog: PathBuf, script: Script) -> (Provider, String) {
    // Hold the first turn: by then the harness is up and its MCP server started.
    let gate = TurnGate::holding_from(wire, 1);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog,
            script,
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");
    let url = server.base_url();
    (
        Provider {
            gate,
            _server: server,
        },
        url,
    )
}

fn providers() -> &'static Providers {
    static P: OnceLock<Providers> = OnceLock::new();
    P.get_or_init(|| {
        let home = scratch("token-argv-home");
        let (codex, codex_url) = provider(
            "responses",
            home.join("codex-requests.jsonl"),
            Script {
                child_narrative: NARRATIVE.into(),
                ..Script::default()
            },
        );
        let (qwen, qwen_url) = provider(
            "openai",
            home.join("qwen-requests.jsonl"),
            Script {
                openai_report_tool: "mcp__marion__report".into(),
                openai_report_args: json!({ "narrative": NARRATIVE }),
                ..Script::default()
            },
        );
        let (copilot, copilot_url) = provider(
            "openai",
            home.join("copilot-requests.jsonl"),
            Script {
                openai_report_tool: "marion-report".into(),
                openai_report_args: json!({ "narrative": NARRATIVE }),
                ..Script::default()
            },
        );
        // The operator's own codex configuration: its provider is marion's canned endpoint.
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            format!(
                "model_provider = \"canned\"\n\n[model_providers.canned]\nname = \"canned\"\n\
                 base_url = \"{codex_url}\"\nwire_api = \"responses\"\n"
            ),
        )
        .unwrap();
        let copilot_home = home.join(".copilot");
        std::fs::create_dir_all(&copilot_home).unwrap();
        let mut vars: Vec<(String, String)> = vec![
            ("HOME".into(), home.to_string_lossy().into_owned()),
            // qwen reads its provider from the environment alone.
            ("OPENAI_BASE_URL".into(), qwen_url),
            ("OPENAI_API_KEY".into(), "canned".into()),
            ("OPENAI_MODEL".into(), "canned-1".into()),
            // copilot's BYOK provider, likewise the operator's to set.
            (
                "COPILOT_HOME".into(),
                copilot_home.to_string_lossy().into_owned(),
            ),
            ("COPILOT_PROVIDER_BASE_URL".into(), copilot_url),
            ("COPILOT_PROVIDER_TYPE".into(), "openai".into()),
            ("COPILOT_PROVIDER_WIRE_API".into(), "completions".into()),
            ("COPILOT_PROVIDER_API_KEY".into(), "canned".into()),
            ("COPILOT_MODEL".into(), "canned-1".into()),
            ("COPILOT_OFFLINE".into(), "true".into()),
        ];
        // The ACP row names no update switch of its own (its program is the agent's), so the
        // copilot row's is set here, from the row.
        vars.extend(marion_harness::copilot::SPEC.updates.env());
        // SAFETY: set once, inside `get_or_init`, before any test in this binary has started a
        // process — every test's first act is to call this.
        unsafe {
            for (k, v) in &vars {
                std::env::set_var(k, v);
            }
            for k in ["CODEX_HOME", "QWEN_HOME", TOKEN_ENV] {
                std::env::remove_var(k);
            }
        }
        Providers {
            codex,
            qwen,
            copilot,
            _home: home,
        }
    })
}

fn live_env(root: &Path) -> (PathBuf, Env) {
    let repo = fixture_repo(root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = Env {
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state,
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: None,
        auth: marion_harness::Auth::Inherited,
    };
    (repo, env)
}

/// `(pid, ppid, argv)` for every process on the machine — argv only, never the environment.
fn process_table() -> Vec<(i32, i32, String)> {
    let out = Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,command="])
        .output()
        .expect("`ps` runs");
    assert!(out.status.success(), "`ps` exited {}", out.status);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let rest = l.trim_start();
            let after_pid = rest[rest.find(char::is_whitespace)?..].trim_start();
            let command = after_pid[after_pid.find(char::is_whitespace)?..].trim_start();
            Some((pid, ppid, command.to_string()))
        })
        .collect()
}

/// The value of `MARION_NODE_TOKEN` in a process's environment, as `ps -E` prints it after the
/// argv. The last occurrence, so an argv that also named it cannot stand in for the environment.
/// `pid`'s command line with its environment appended, as `ps -E` prints it.
fn env_text_of(pid: i32) -> String {
    let out = Command::new("ps")
        .args(["-Eww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .expect("`ps -E` runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The value `pid`'s environment gives `name`, if any.
fn env_of(pid: i32, name: &str) -> Option<String> {
    let text = env_text_of(pid);
    let needle = format!("{name}=");
    let at = text.rfind(&needle)?;
    let value: String = text[at + needle.len()..]
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    (!value.is_empty()).then_some(value)
}

/// The node's owner, as the supervisor's `agent/spawn` is: it mints the token the declaration and
/// environment carry, and learns the pid of the process marion started.
struct Owner {
    token: String,
    pid: Mutex<Option<i32>>,
}

impl SpawnObserver for Owner {
    fn identified(&self, _: &AgentId) -> Option<Secret> {
        Some(self.token.as_str().into())
    }
    fn started(&self, _: &AgentId, pid: i32) {
        *self.pid.lock().unwrap() = Some(pid);
    }
}

/// Spawn `agent_type` live, hold its first provider turn, read the process table while it is
/// held, release, and return the contract.
fn spawn_and_inspect(tag: &str, agent_type: &str, provider: &Provider) -> TaskContract {
    let root = scratch(tag);
    let (repo, env) = live_env(&root);
    let req = SpawnRequest {
        budget: None,
        review: None,
        agent_type: agent_type.into(),
        prompt: format!(
            "Call the `report` tool of the `marion` MCP server exactly once with the narrative \
             \"{NARRATIVE}\". Do nothing else."
        ),
        repo,
        acceptance_criteria: vec!["the node reported".into()],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        race: None,
        read_only: false,
        workflow: None,
    };
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let task = TaskId(format!("{tag}-1"));
    // The owner's hooks: a known token for the node, and the pid of the process it started.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let observer = Arc::new(Owner {
        token: format!("{:0>64}", format!("{:x}{nanos:x}", std::process::id())),
        pid: Mutex::new(None),
    });
    let run = {
        let observer = Arc::clone(&observer);
        std::thread::spawn(move || {
            run_spawn_watched(&env, &req, &task, &caller.clone().into(), observer.as_ref())
                .map_err(|e| e.to_string())
        })
    };

    let parked = until_within(Duration::from_secs(90), Duration::from_millis(50), || {
        provider.gate.parked() > 0 || run.is_finished()
    });
    assert!(
        parked && provider.gate.parked() > 0,
        "{agent_type}: the harness never asked the provider for its first turn"
    );

    // Held: the harness is alive, and so is the bridge it started. A failed check kills the
    // node's processes before it unwinds, so a held harness does not outlive the test.
    let inspected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let started = observer
            .pid
            .lock()
            .unwrap()
            .expect("the owner saw the process start");
        let bridge = env!("CARGO_BIN_EXE_marion-supervisor");
        let table = process_table();
        let mut tree = vec![started];
        while let Some(next) = table
            .iter()
            .find(|(pid, ppid, _)| tree.contains(ppid) && !tree.contains(pid))
            .map(|(pid, _, _)| *pid)
        {
            tree.push(next);
        }
        let bridges: Vec<i32> = table
            .iter()
            .filter(|(pid, _, argv)| tree.contains(pid) && argv.starts_with(bridge))
            .map(|(pid, _, _)| *pid)
            .collect();
        let token = observer.token.as_str();
        for (pid, _, argv) in &table {
            assert!(
                !argv.contains(token),
                "{agent_type}: the node token is on the argv of process {pid}: {argv}"
            );
            // Naming the variable for a pass-through list (codex's `env_vars`) is fine; binding it
            // to a value, in either spelling a declaration uses, is not.
            let binds = [format!("{TOKEN_ENV}="), format!("\"{TOKEN_ENV}\":")];
            assert!(
                !(tree.contains(pid) && binds.iter().any(|b| argv.contains(b.as_str()))),
                "{agent_type}: a node process's argv binds the token variable: {argv}"
            );
        }

        assert!(
            !env_text_of(started).contains(token),
            "{agent_type}: the node token is in the harness's environment"
        );
        assert_eq!(env_of(started, TOKEN_ENV), None, "{agent_type}");
        let file = env_of(started, TOKEN_FILE_ENV).unwrap_or_else(|| {
            panic!("{agent_type}: the harness's environment names no token file")
        });
        assert_eq!(
            std::fs::read_to_string(&file).unwrap_or_default(),
            token,
            "{agent_type}: the named file holds the node's token"
        );
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{agent_type}: {file} is private");
        }
        assert!(
            !bridges.is_empty(),
            "{agent_type}: the harness started no bridge: {:?}",
            table
                .iter()
                .filter(|(p, _, _)| tree.contains(p))
                .collect::<Vec<_>>()
        );
        for b in &bridges {
            assert!(
                !env_text_of(*b).contains(token),
                "{agent_type}: the bridge {b}'s environment holds the token itself"
            );
            assert_eq!(
                env_of(*b, TOKEN_FILE_ENV).as_deref(),
                Some(file.as_str()),
                "{agent_type}: the bridge {b} was handed the token's file"
            );
        }
    }));
    if let Err(why) = inspected {
        sweep(&root.to_string_lossy());
        std::panic::resume_unwind(why);
    }
    provider.gate.release();
    let contract = run
        .join()
        .expect("run_spawn does not panic")
        .unwrap_or_else(|e| panic!("{agent_type}: the live child failed: {e}"));
    let report = contract
        .completion
        .as_ref()
        .unwrap_or_else(|| panic!("{agent_type}: the node never reached marion's report"));
    assert!(
        format!("{report:?}").contains(NARRATIVE),
        "{agent_type}: the report marion received is the node's: {report:?}"
    );
    contract
}

#[test]
fn a_live_codex_nodes_token_is_on_its_environment_and_never_its_argv() {
    let p = providers();
    if !harness_available("codex") {
        return;
    }
    spawn_and_inspect("token-argv-codex", "codex-impl", &p.codex);
}

#[test]
fn a_live_qwen_nodes_token_is_on_its_environment_and_never_its_argv() {
    let p = providers();
    if !harness_available("qwen") {
        return;
    }
    spawn_and_inspect("token-argv-qwen", "qwen-orchestrator", &p.qwen);
}

#[test]
fn a_copilot_acp_nodes_token_is_on_its_environment_and_never_its_argv() {
    let p = providers();
    if !harness_available("copilot") {
        return;
    }
    spawn_and_inspect("token-argv-copilot", "acp:copilot", &p.copilot);
}
