//! **Endpoint mode, end to end**: a node whose model names a provider is pointed at that provider
//! with the key the user stored — here, marion's own canned server standing in as the provider.
//!
//! # Why a test binary of its own
//!
//! A launch reads the user's registry and credential store from the process environment
//! (`XDG_CONFIG_HOME`, `MARION_CREDENTIAL_STORE`), exactly as a supervisor does. This binary sets
//! both once, to a fixture directory it writes itself — a `credentials.json` holding the fixture key
//! `sk-endpoint-test` and a `providers.toml` naming `canned-test` — so no test here can read the
//! user's real configuration, touch the Keychain, or reach a real provider. Putting these cells in
//! `harness_matrix` would hand that environment to its canned cells' children too.
//!
//! Cells are serialized: `providers.toml` points `canned-test` at one cell's canned server at a
//! time, since each server binds its own port.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use marion_core::contract::{Isolation, TaskId};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, scratch};

/// The fixture key. The test writes it; nothing in marion ever mints it.
const KEY: &str = "sk-endpoint-test";
/// Two labelled keys of `canned-rot`, tried in the stated order `a` then `b`.
const KEY_A: &str = "sk-rotate-key-a-0001";
const KEY_B: &str = "sk-rotate-key-b-0002";

struct Fixture {
    config: PathBuf,
    /// Held by a `static`, so never dropped: the dir outlives this process and the next test run's
    /// first `scratch` reclaims it, which is what `marion_testsupport` does with an orphan.
    _root: marion_testsupport::Scratch,
}

/// The one fixture config dir for this binary, with the environment pointed at it.
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = scratch("endpoint-config");
        let config = root.join("marion");
        std::fs::create_dir_all(&config).unwrap();
        write_owner_only(
            &config.join("credentials.json"),
            &format!(
                "{{\"providers\": {{\"canned-test\": \"{KEY}\", \"canned-anthropic\": \"{KEY}\", \
                 \"canned-anthropic-bearer\": \"{KEY}\", \"canned-chat-xkey\": \"{KEY}\", \
                 \"canned-chat\": \"{KEY}\", \
                 \"canned-rot:a\": \"{KEY_A}\", \"canned-rot:b\": \"{KEY_B}\"}}}}\n"
            ),
        );
        // SAFETY: set once, inside `get_or_init`, before any test in this binary has read the
        // environment or started a process — every test's first act is to call this.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &*root);
            std::env::set_var("MARION_CREDENTIAL_STORE", "file");
        }
        Fixture {
            config,
            _root: root,
        }
    })
}

fn write_owner_only(path: &Path, body: &str) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    f.write_all(body.as_bytes()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Hold the cell lock and point the fixture providers at `base_url`: `canned-test` serving all
/// four wires, `canned-chat` serving Chat Completions alone, `canned-anthropic` serving Anthropic
/// Messages alone and reading its key from `x-api-key` as Anthropic's own API does,
/// `canned-anthropic-bearer` the same wire reading a Bearer key as the gateways do, `canned-nokey`
/// with no stored key, `canned-chat-xkey` serving Chat Completions and reading `x-api-key`,
/// `canned-rot` holding two labelled keys in a stated order.
fn providers_at(base_url: &str) -> MutexGuard<'static, ()> {
    static CELLS: Mutex<()> = Mutex::new(());
    let guard = CELLS.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    std::fs::write(
        fx.config.join("providers.toml"),
        format!(
            "[providers.canned-test]\nbase_url = \"{base_url}\"\n\
             wires = [\"anthropic\", \"openai-chat\", \"openai-responses\", \"gemini\"]\n\n\
             [providers.canned-chat]\nbase_url = \"{base_url}\"\nwires = [\"openai-chat\"]\n\n\
             [providers.canned-nokey]\nbase_url = \"{base_url}\"\nwires = [\"openai-chat\", \
             \"openai-responses\"]\n\n\
             [providers.canned-anthropic]\nbase_url = \"{root}\"\nwires = [\"anthropic\"]\n\
             key_header = \"x-api-key\"\n\n\
             [providers.canned-anthropic-bearer]\nbase_url = \"{root}\"\nwires = [\"anthropic\"]\n\n\
             [providers.canned-chat-xkey]\nbase_url = \"{base_url}\"\nwires = [\"openai-chat\"]\n\
             key_header = \"x-api-key\"\n\n\
             [providers.canned-rot]\nbase_url = \"{base_url}\"\n\
             wires = [\"openai-chat\", \"openai-responses\"]\n\n\
             [credentials]\ncanned-rot = [\"canned-rot:a\", \"canned-rot:b\"]\n",
            // An Anthropic base is the root, as the seed rows spell it: the SDK appends `/v1`.
            root = base_url.trim_end_matches("/v1")
        ),
    )
    .unwrap();
    guard
}

struct Tree {
    _root: marion_testsupport::Scratch,
    repo: PathBuf,
    state: PathBuf,
    env: Env,
}

fn tree(tag: &str, base_url: Option<String>) -> Tree {
    let root = scratch(&format!("endpoint-{tag}"));
    let repo = fixture_repo(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = canned_env(&state, &repo, base_url);
    Tree {
        _root: root,
        repo,
        state,
        env,
    }
}

fn request(t: &Tree, agent_type: &str, model: &str) -> SpawnRequest {
    SpawnRequest {
        review: None,
        agent_type: agent_type.into(),
        prompt: "Report back through marion.".into(),
        repo: t.repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: 60,
        model: Some(model.into()),
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        race: None,
    }
}

/// A refusal comes before the node exists: no journal record at all, no contract.
fn assert_refused_before_the_node_existed(t: &Tree, err: &str, needles: &[&str]) {
    for n in needles {
        assert!(err.contains(n), "the refusal must name {n:?}: {err}");
    }
    assert!(!err.contains(KEY), "{err}");
    let journal = marion_supervisor::journal::read(&t.env.project_dir).unwrap();
    assert_eq!(journal.records, 0, "a refused endpoint journals nothing");
    assert!(
        marion_testsupport::persisted_contracts(&t.state)
            .unwrap_or_default()
            .is_empty()
    );
}

fn spawn_err(t: &Tree, agent_type: &str, model: &str) -> String {
    run_spawn(
        &t.env,
        &request(t, agent_type, model),
        &TaskId(format!("endpoint-{agent_type}")),
        &Caller::root(
            "root",
            marion_core::agent_type::builtin("claude").expect("the root type resolves"),
        ),
    )
    .expect_err("an unusable endpoint is refused")
    .to_string()
}

#[test]
fn a_provider_with_no_stored_key_is_refused_by_name_with_the_login_command() {
    let t = tree("nokey", Some("http://127.0.0.1:9/v1".into()));
    let _cells = providers_at("http://127.0.0.1:9/v1");
    let err = spawn_err(&t, "codex-impl", "canned-nokey:some-model");
    assert_refused_before_the_node_existed(&t, &err, &["marion login canned-nokey"]);
}

#[test]
fn a_provider_sharing_no_wire_with_the_harness_is_refused_naming_both() {
    let t = tree("nowire", Some("http://127.0.0.1:9/v1".into()));
    let _cells = providers_at("http://127.0.0.1:9/v1");
    let err = spawn_err(&t, "codex-impl", "canned-chat:some-model");
    assert_refused_before_the_node_existed(
        &t,
        &err,
        &["codex", "openai-responses", "canned-chat", "openai-chat"],
    );
}

#[test]
fn an_acp_type_naming_a_provider_is_refused_because_acp_has_no_endpoint_wire() {
    let t = tree("acp", Some("http://127.0.0.1:9/v1".into()));
    let _cells = providers_at("http://127.0.0.1:9/v1");
    let err = spawn_err(&t, "acp-opencode", "canned-test:some-model");
    assert_refused_before_the_node_existed(&t, &err, &["no endpoint wire"]);
}

// ---- the cells: a real harness, pointed at the fixture provider --------------------------------

use marion_core::contract::{ExitStatus, TaskContract};
use marion_provider::{CannedServer, Config, Script, reqlog::fingerprint};
use marion_testsupport::{
    judge, kill_hard, on_path, persisted_contracts, pinned_version, survivors,
};
use serde_json::{Value, json};

mod common;

use common::canned::canned_env;

/// The model every cell asks the provider for, through the `canned-test:` prefix.
const MODEL: &str = "endpoint-model-7";
const NARRATIVE: &str = "Reported back through marion from an endpoint node.";

/// How the harness presents the key: `Authorization: Bearer <key>`, the Anthropic SDK's
/// `x-api-key: <key>`, or the Gemini wire's `x-goog-api-key: <key>`.
#[derive(Clone, Copy)]
enum Presents {
    Bearer,
    XApiKey,
    GoogApiKey,
}

struct Cell {
    presents: Presents,
    agent_type: &'static str,
    /// The fixture provider the model names.
    provider: &'static str,
    script: Script,
    /// The model the contract records: the harness's own spelling of [`MODEL`].
    compiled_model: &'static str,
    wire: &'static str,
    /// `native`, or `translated` where marion's gateway bridges the harness to the provider.
    route: &'static str,
}

struct Evidence {
    contract: Result<TaskContract, String>,
    persisted: Vec<Value>,
    requests: Vec<Value>,
    journal: String,
    leaked: Vec<String>,
    /// Every file under the tree's state directory — journal, contracts, event logs, config
    /// documents — as `(path, bytes)`, for a search for a key.
    kept: Vec<(PathBuf, Vec<u8>)>,
}

fn drive(cell: &Cell) -> Evidence {
    drive_held(cell, None)
}

/// Every regular file under `dir`, recursively.
fn every_file(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => every_file(&path, out),
            Ok(t) if t.is_file() => {
                if let Ok(bytes) = std::fs::read(&path) {
                    out.push((path, bytes));
                }
            }
            _ => {}
        }
    }
}

fn drive_held(cell: &Cell, hold: Option<std::sync::Arc<dyn marion_provider::Hold>>) -> Evidence {
    let reqlog_dir = scratch(&format!("endpoint-reqlog-{}", cell.agent_type));
    let server = CannedServer::start_held(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: reqlog_dir.join("provider-requests.jsonl"),
            script: cell.script.clone(),
        },
        hold,
    )
    .expect("the canned provider binds");
    let base_url = server.base_url();
    let _cells = providers_at(&base_url);
    let t = tree(cell.agent_type, Some(base_url));
    let contract = run_spawn(
        &t.env,
        &request(&t, cell.agent_type, &format!("{}:{MODEL}", cell.provider)),
        &TaskId(format!("endpoint-{}", cell.agent_type)),
        &Caller::root(
            "root",
            marion_core::agent_type::builtin("claude").expect("the root type resolves"),
        ),
    )
    .map_err(|e| e.to_string());
    let requests = server.requests().unwrap_or_default();
    let walked = persisted_contracts(&t.state);
    let journal = std::fs::read_to_string(t.env.project_dir.journal()).unwrap_or_default();
    drop(server);
    let leaked = survivors(&t.state.parent().unwrap().to_string_lossy());
    for (pid, _) in &leaked {
        kill_hard(*pid);
    }
    let walked = walked.expect("the state dir walks");
    let persisted = judge(&walked).into_iter().map(|(_, v)| v.clone()).collect();
    let mut kept = Vec::new();
    every_file(&t.state, &mut kept);
    Evidence {
        contract,
        persisted,
        requests,
        journal,
        leaked: leaked.into_iter().map(|(_, l)| l).collect(),
        kept,
    }
}

fn summary(ev: &Evidence) -> String {
    ev.requests
        .iter()
        .map(|r| {
            format!(
                "  seq {} {} {} wire {:?} model {:?} credentials {}",
                r["seq"], r["method"], r["path"], r["wire"], r["body"]["model"], r["credentials"]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The model a logged request asks for: the body's `model`, else the Gemini path's
/// `/models/<model>:<method>` segment.
fn requested_model(r: &Value) -> Option<String> {
    if let Some(m) = r["body"]["model"].as_str() {
        return Some(m.to_string());
    }
    let path = r["path"].as_str()?;
    let rest = path.split("/models/").nth(1)?;
    Some(rest.split(':').next()?.to_string())
}

fn assert_endpoint_cell(cell: &Cell, ev: &Evidence) {
    let who = cell.agent_type;
    let contract = ev
        .contract
        .as_ref()
        .unwrap_or_else(|e| panic!("{who}: run_spawn failed: {e}\n{}", summary(ev)));
    assert!(!ev.requests.is_empty(), "{who}: the provider saw nothing");
    let (header, value) = match cell.presents {
        Presents::Bearer => ("authorization", fingerprint(&format!("Bearer {KEY}"))),
        Presents::XApiKey => ("x-api-key", fingerprint(KEY)),
        Presents::GoogApiKey => ("x-goog-api-key", fingerprint(KEY)),
    };
    for r in &ev.requests {
        // claude 2.1.283 opens with `HEAD /api/hello` against the base URL, a reachability probe
        // that carries no credential and no body: nothing to check, and nothing that could leak.
        if r["method"] == "HEAD" && r["credentials"].is_null() && r["body"].is_null() {
            continue;
        }
        // The user's stored key on every request — never a placeholder.
        assert_eq!(
            r["credentials"][header],
            value,
            "{who}: every request presents the stored key in `{header}`\n{}",
            summary(ev)
        );
        // And no second credential beside it.
        let others: Vec<&String> = r["credentials"]
            .as_object()
            .map(|m| m.keys().filter(|k| *k != header).collect())
            .unwrap_or_default();
        assert!(others.is_empty(), "{who}: a second credential {others:?}");
        // No subscription login: no OAuth marker anywhere in what the harness sent.
        let headers = r["headers"].to_string().to_ascii_lowercase();
        assert!(
            !headers.contains("oauth"),
            "{who}: an OAuth header: {headers}"
        );
        // No request names any model but the chosen one — background calls included. The Gemini
        // wire names it in the path (`/models/<model>:streamGenerateContent`), the others in the body.
        if let Some(m) = requested_model(r).as_deref() {
            assert_eq!(
                m,
                MODEL,
                "{who}: a request named another model\n{}",
                summary(ev)
            );
        }
        assert_eq!(r["wire"], cell.wire, "{who}: wire\n{}", summary(ev));
    }
    assert!(
        ev.requests
            .iter()
            .any(|r| requested_model(r).as_deref() == Some(MODEL)),
        "{who}: no request named the model at all\n{}",
        summary(ev)
    );
    let comp = contract.completion.as_ref().expect("a finished run");
    assert_eq!(
        comp.status,
        ExitStatus::Ok,
        "{who}: {}",
        comp.exit.description
    );
    assert!(
        comp.narrative
            .as_ref()
            .is_some_and(|n| n.value == NARRATIVE),
        "{who}: the child reported through marion's bridge"
    );
    assert_eq!(
        contract.child.model.as_deref(),
        Some(cell.compiled_model),
        "{who}"
    );
    assert_eq!(
        contract.child.provider.as_deref(),
        Some(cell.provider),
        "{who}"
    );
    assert_eq!(contract.child.route.as_deref(), Some(cell.route), "{who}");
    // The credential by id — the provider's unlabelled one here — and never the key.
    assert_eq!(
        contract.child.credential.as_deref(),
        Some(cell.provider),
        "{who}"
    );
    assert_eq!(ev.persisted.len(), 1, "{who}");
    // The key is in no record marion keeps.
    for (what, text) in [
        ("the persisted contract", ev.persisted[0].to_string()),
        ("the journal", ev.journal.clone()),
    ] {
        assert!(!text.contains(KEY), "{who}: the key is in {what}");
    }
    assert!(
        ev.journal
            .contains(&format!("\"provider\":\"{}\"", cell.provider)),
        "{who}: the journal's Spawned names the provider"
    );
    assert!(ev.leaked.is_empty(), "{who}: leaked {:?}", ev.leaked);
}

#[test]
fn a_claude_code_child_runs_on_the_users_provider_over_the_anthropic_wire() {
    assert!(
        on_path("claude"),
        "put `claude` ({}) on PATH",
        pinned_version("claude")
    );
    let cell = Cell {
        presents: Presents::Bearer,
        provider: "canned-test",
        agent_type: "claude",
        script: Script {
            root_tool: "mcp__marion__report".into(),
            root_tool_input: json!({ "narrative": NARRATIVE }),
            root_final_text: "Reported. Done.".into(),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "anthropic",
        route: "native",
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}

#[test]
fn a_codex_child_runs_on_the_users_provider_over_the_responses_wire() {
    assert!(
        on_path("codex"),
        "put `codex` ({}) on PATH",
        pinned_version("codex")
    );
    let cell = Cell {
        presents: Presents::Bearer,
        provider: "canned-test",
        agent_type: "codex-impl",
        script: Script {
            child_narrative: NARRATIVE.into(),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "responses",
        route: "native",
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}

#[test]
fn an_opencode_child_runs_on_the_users_provider_over_the_chat_wire() {
    assert!(
        on_path("opencode"),
        "put `opencode` ({}) on PATH",
        pinned_version("opencode")
    );
    let cell = Cell {
        presents: Presents::Bearer,
        provider: "canned-test",
        agent_type: "opencode",
        script: Script {
            openai_report_tool: "marion_report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        compiled_model: "marion/endpoint-model-7",
        wire: "openai",
        route: "native",
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}

#[test]
fn a_copilot_child_runs_on_the_users_provider_over_the_chat_wire() {
    assert!(
        on_path("copilot"),
        "put `copilot` ({}) on PATH",
        pinned_version("copilot")
    );
    let cell = Cell {
        presents: Presents::Bearer,
        provider: "canned-test",
        agent_type: "copilot",
        script: Script {
            openai_report_tool: "marion-report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "openai",
        route: "native",
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}

/// **The same harness on a second wire, chosen by the same resolver from row data alone.** A
/// provider serving only Anthropic Messages leaves copilot's Chat recipe unusable, so resolution
/// takes the row's next recipe — `COPILOT_PROVIDER_TYPE=anthropic` — and nothing in the resolver
/// names copilot.
#[test]
fn a_copilot_child_takes_its_anthropic_recipe_when_the_provider_serves_only_that_wire() {
    assert!(
        on_path("copilot"),
        "put `copilot` ({}) on PATH",
        pinned_version("copilot")
    );
    let cell = Cell {
        // The Anthropic SDK's own header, which is what api.anthropic.com itself reads.
        presents: Presents::XApiKey,
        provider: "canned-anthropic",
        agent_type: "copilot",
        script: Script {
            root_tool: "marion-report".into(),
            root_tool_input: json!({ "narrative": NARRATIVE }),
            root_final_text: "Reported. Done.".into(),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "anthropic",
        route: "native",
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}

/// **The key header is the provider's, and the recipe honors it**: the same copilot anthropic
/// recipe, against a provider that reads a Bearer key, presents `Authorization: Bearer` through
/// `COPILOT_PROVIDER_BEARER_TOKEN` instead of the `x-api-key` the cell above sees.
#[test]
fn a_copilot_child_presents_a_bearer_key_to_an_anthropic_provider_that_reads_one() {
    assert!(
        on_path("copilot"),
        "put `copilot` ({}) on PATH",
        pinned_version("copilot")
    );
    let cell = Cell {
        presents: Presents::Bearer,
        provider: "canned-anthropic-bearer",
        agent_type: "copilot",
        script: Script {
            root_tool: "marion-report".into(),
            root_tool_input: json!({ "narrative": NARRATIVE }),
            root_final_text: "Reported. Done.".into(),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "anthropic",
        route: "native",
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}

/// **An OpenAI-wire provider that reads `x-api-key`**: opencode's generated provider block carries
/// the key as a header and sends no Bearer beside it.
#[test]
fn an_opencode_child_presents_an_x_api_key_to_a_chat_provider_that_reads_one() {
    assert!(
        on_path("opencode"),
        "put `opencode` ({}) on PATH",
        pinned_version("opencode")
    );
    let cell = Cell {
        presents: Presents::XApiKey,
        provider: "canned-chat-xkey",
        agent_type: "opencode",
        script: Script {
            openai_report_tool: "marion_report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        compiled_model: "marion/endpoint-model-7",
        wire: "openai",
        route: "native",
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}

// ---- the gateway: a harness pointed at a provider that serves another wire ---------------------

/// A hold that, on the first scripted turn, snapshots the argv of every process descended from this
/// test — while the gateway's curl for that turn is still waiting on the provider, and the harness
/// on the gateway.
#[derive(Debug, Default)]
struct ArgvWitness {
    seen: Mutex<Option<String>>,
}

impl marion_provider::Hold for ArgvWitness {
    fn wait_for(&self, wire: Option<&str>, body: &Value) {
        let turn = body["tools"].as_array().is_some_and(|t| !t.is_empty());
        let mut seen = self.seen.lock().unwrap();
        if wire == Some("openai") && turn && seen.is_none() {
            *seen = Some(descendant_argv());
        }
    }
}

/// `pid ppid args` of every process, kept for the descendants of this test process.
fn descendant_argv() -> String {
    let out = std::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,args="])
        .output()
        .expect("ps runs");
    let rows: Vec<(u32, u32, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            Some((pid, ppid, it.collect::<Vec<_>>().join(" ")))
        })
        .collect();
    let mut ours = vec![std::process::id()];
    let mut grew = true;
    while grew {
        grew = false;
        for (pid, ppid, _) in &rows {
            if ours.contains(ppid) && !ours.contains(pid) {
                ours.push(*pid);
                grew = true;
            }
        }
    }
    rows.iter()
        .filter(|(pid, _, _)| ours.contains(pid) && *pid != std::process::id())
        .map(|(_, _, args)| args.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// **Claude Code on a provider that serves only Chat Completions, through marion's gateway**: a
/// real `claude` child speaks Anthropic Messages to a gateway marion started for it, which sends a
/// streamed Chat Completions turn to the provider with the stored key, and the provider's streamed
/// `tool_calls` come back as the `tool_use` that reports through marion's bridge.
///
/// Beside the cell's own checks: the key is on no process's argv while a turn is in flight (nor is
/// the gateway's bearer), it is in no file the tree keeps — journal, contract, event log, config
/// documents — and the gateway is gone once the node has exited.
#[test]
fn a_claude_code_child_runs_on_a_chat_only_provider_through_marions_gateway() {
    assert!(
        on_path("claude"),
        "put `claude` ({}) on PATH",
        pinned_version("claude")
    );
    let cell = Cell {
        presents: Presents::Bearer,
        provider: "canned-chat",
        agent_type: "claude",
        script: Script {
            openai_report_tool: "mcp__marion__report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "openai",
        route: "translated",
    };
    let witness = std::sync::Arc::new(ArgvWitness::default());
    let started = marion_supervisor::gateway::started();
    let ev = drive_held(&cell, Some(witness.clone()));
    assert_endpoint_cell(&cell, &ev);
    assert!(
        marion_supervisor::gateway::started() > started,
        "the node ran through a gateway"
    );
    assert_eq!(
        marion_supervisor::gateway::live(),
        0,
        "the gateway is gone once the node has exited"
    );
    // A real turn went through: the provider saw the tool's call answered in the transcript.
    assert!(
        ev.requests.iter().any(|r| r["body"]["messages"]
            .as_array()
            .is_some_and(|m| m.iter().any(|m| m["role"] == "tool"))),
        "no request carried the tool's result\n{}",
        summary(&ev)
    );
    let argv = witness
        .seen
        .lock()
        .unwrap()
        .clone()
        .expect("a scripted turn reached the provider");
    assert!(
        argv.contains("curl") && argv.contains("@/dev/fd/3"),
        "the snapshot was taken while the gateway's curl was in flight:\n{argv}"
    );
    assert!(argv.contains("claude"), "{argv}");
    assert!(!argv.contains(KEY), "the key is on an argv:\n{argv}");
    assert!(
        !argv.contains("marion-gw-"),
        "the gateway's bearer is on an argv:\n{argv}"
    );
    let kept: Vec<&PathBuf> = ev
        .kept
        .iter()
        .filter(|(_, bytes)| bytes.windows(KEY.len()).any(|w| w == KEY.as_bytes()))
        .map(|(p, _)| p)
        .collect();
    assert!(kept.is_empty(), "the key is kept in {kept:?}");
    assert!(
        ev.kept
            .iter()
            .any(|(p, _)| p.file_name().is_some_and(|n| n == "events.jsonl")),
        "the node's event log was among the files searched"
    );
    assert!(
        ev.journal.contains("\"route\":\"translated\""),
        "the journal's Spawned names the route"
    );
}

/// **The other translation: opencode (Chat Completions alone) on a provider that serves only
/// Anthropic Messages** and reads `x-api-key`, as Anthropic's own API does. The gateway sends each
/// streamed turn as Anthropic Messages with the stored key in that header, and the provider's
/// `tool_use` comes back as the Chat `tool_calls` that report through marion's bridge.
#[test]
fn an_opencode_child_runs_on_an_anthropic_only_provider_through_marions_gateway() {
    assert!(
        on_path("opencode"),
        "put `opencode` ({}) on PATH",
        pinned_version("opencode")
    );
    let cell = Cell {
        presents: Presents::XApiKey,
        provider: "canned-anthropic",
        agent_type: "opencode",
        script: Script {
            root_tool: "marion_report".into(),
            root_tool_input: json!({ "narrative": NARRATIVE }),
            root_final_text: "Reported. Done.".into(),
            ..Script::default()
        },
        compiled_model: "marion/endpoint-model-7",
        wire: "anthropic",
        route: "translated",
    };
    let started = marion_supervisor::gateway::started();
    let ev = drive(&cell);
    assert_endpoint_cell(&cell, &ev);
    assert!(marion_supervisor::gateway::started() > started);
    assert_eq!(marion_supervisor::gateway::live(), 0);
    let kept: Vec<&PathBuf> = ev
        .kept
        .iter()
        .filter(|(_, bytes)| bytes.windows(KEY.len()).any(|w| w == KEY.as_bytes()))
        .map(|(p, _)| p)
        .collect();
    assert!(
        kept.is_empty(),
        "the key is kept in {kept:?} — on a translated route not even opencode's config holds it"
    );
}

/// A Chat-only harness on `canned-anthropic` through the gateway, reporting under its own spelling
/// of marion's tool — the same cell as opencode's, for the rows with a Chat recipe alone.
fn chat_harness_via_gateway(agent_type: &'static str, report_tool: &str) {
    assert!(
        on_path(agent_type),
        "put `{agent_type}` ({}) on PATH",
        pinned_version(agent_type)
    );
    let cell = Cell {
        presents: Presents::XApiKey,
        provider: "canned-anthropic",
        agent_type,
        script: Script {
            root_tool: report_tool.into(),
            root_tool_input: json!({ "narrative": NARRATIVE }),
            root_final_text: "Reported. Done.".into(),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "anthropic",
        route: "translated",
    };
    let ev = drive(&cell);
    assert_endpoint_cell(&cell, &ev);
    assert_eq!(marion_supervisor::gateway::live(), 0);
}

#[test]
fn a_goose_child_runs_on_an_anthropic_only_provider_through_marions_gateway() {
    chat_harness_via_gateway("goose", "marion__report");
}

#[test]
fn a_cline_child_runs_on_an_anthropic_only_provider_through_marions_gateway() {
    chat_harness_via_gateway("cline", "marion__report");
}

#[test]
fn a_qwen_child_runs_on_an_anthropic_only_provider_through_marions_gateway() {
    chat_harness_via_gateway("qwen", "mcp__marion__report");
}

// ---- credential rotation: API keys only, before the first successful turn ----------------------

/// A codex child on `canned-rot`, whose provider refuses the keys in `refused` with `status`.
fn drive_rotation(tag: &str, refused: &[&str], status: u16) -> (Evidence, Result<Value, String>) {
    let reqlog_dir = scratch(&format!("endpoint-reqlog-rot-{tag}"));
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: reqlog_dir.join("provider-requests.jsonl"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            refusals: refused
                .iter()
                .map(|k| marion_provider::KeyRefusal {
                    key: k.to_string(),
                    status,
                })
                .collect(),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let base_url = server.base_url();
    let _cells = providers_at(&base_url);
    let t = tree(&format!("rot-{tag}"), Some(base_url));
    let contract = run_spawn(
        &t.env,
        &request(&t, "codex-impl", &format!("canned-rot:{MODEL}")),
        &TaskId(format!("endpoint-rot-{tag}")),
        &Caller::root(
            "root",
            marion_core::agent_type::builtin("claude").expect("the root type resolves"),
        ),
    )
    .map_err(|e| e.to_string());
    let as_json = contract
        .as_ref()
        .map(|c| serde_json::to_value(c).unwrap())
        .map_err(Clone::clone);
    let requests = server.requests().unwrap_or_default();
    let walked = persisted_contracts(&t.state);
    let journal = std::fs::read_to_string(t.env.project_dir.journal()).unwrap_or_default();
    drop(server);
    let leaked = survivors(&t.state.parent().unwrap().to_string_lossy());
    for (pid, _) in &leaked {
        kill_hard(*pid);
    }
    let persisted = judge(&walked.expect("the state dir walks"))
        .into_iter()
        .map(|(_, v)| v.clone())
        .collect();
    (
        Evidence {
            contract,
            persisted,
            requests,
            journal,
            leaked: leaked.into_iter().map(|(_, l)| l).collect(),
            kept: Vec::new(),
        },
        as_json,
    )
}

fn presented(r: &Value, key: &str) -> bool {
    r["credentials"]["authorization"] == fingerprint(&format!("Bearer {key}"))
}

fn assert_no_key_kept(ev: &Evidence) {
    for key in [KEY_A, KEY_B] {
        for (what, text) in [
            (
                "the persisted contract",
                ev.persisted
                    .iter()
                    .map(Value::to_string)
                    .collect::<String>(),
            ),
            ("the journal", ev.journal.clone()),
        ] {
            assert!(!text.contains(key), "the key is in {what}");
        }
    }
    assert!(ev.leaked.is_empty(), "leaked {:?}", ev.leaked);
}

/// **A rate-limited first key rotates to the next, before any turn succeeded**: the provider
/// answers key `a` with 429, so the node is relaunched fresh on `b`, succeeds there, and its
/// contract records the failover by id — never a key.
#[test]
fn a_rate_limited_key_fails_over_to_the_next_stated_credential_before_the_first_turn() {
    assert!(
        on_path("codex"),
        "put `codex` ({}) on PATH",
        pinned_version("codex")
    );
    let (ev, json) = drive_rotation("429", &[KEY_A], 429);
    let contract = ev
        .contract
        .as_ref()
        .unwrap_or_else(|e| panic!("run_spawn failed: {e}\n{}", summary(&ev)));
    let comp = contract.completion.as_ref().expect("a finished run");
    assert_eq!(
        comp.status,
        ExitStatus::Ok,
        "{}\n{}",
        comp.exit.description,
        summary(&ev)
    );
    assert_eq!(contract.child.credential.as_deref(), Some("canned-rot:b"));
    // One move, by id, with the one classifier's cause and the harness's own sentence.
    let moves = json.unwrap()["child"]["credential_failover"].clone();
    assert_eq!(moves.as_array().map(Vec::len), Some(1), "{moves}");
    assert_eq!(
        (&moves[0]["from"], &moves[0]["to"]),
        (&json!("canned-rot:a"), &json!("canned-rot:b"))
    );
    assert!(
        moves[0]["cause"]["RateLimit"]["line"]
            .as_str()
            .is_some_and(|l| l.contains("429")),
        "{moves}"
    );
    let first_b = ev
        .requests
        .iter()
        .position(|r| presented(r, KEY_B))
        .unwrap_or_else(|| panic!("key b was never presented\n{}", summary(&ev)));
    assert!(
        first_b > 0 && presented(&ev.requests[0], KEY_A),
        "a first\n{}",
        summary(&ev)
    );
    assert!(
        ev.requests[first_b..].iter().all(|r| presented(r, KEY_B)),
        "after the failover only b is presented\n{}",
        summary(&ev)
    );
    assert_no_key_kept(&ev);
}

/// **Rotation is bounded**: every stated key refused, each is tried once and the node fails,
/// naming the one failover it took.
#[test]
fn rotation_tries_each_credential_once_and_then_fails() {
    assert!(
        on_path("codex"),
        "put `codex` ({}) on PATH",
        pinned_version("codex")
    );
    let (ev, json) = drive_rotation("all-401", &[KEY_A, KEY_B], 401);
    let contract = ev
        .contract
        .as_ref()
        .unwrap_or_else(|e| panic!("run_spawn failed: {e}\n{}", summary(&ev)));
    let comp = contract.completion.as_ref().expect("a finished run");
    assert_ne!(comp.status, ExitStatus::Ok);
    // One move, by id, with the one classifier's cause and the harness's own sentence.
    let moves = json.unwrap()["child"]["credential_failover"].clone();
    assert_eq!(moves.as_array().map(Vec::len), Some(1), "{moves}");
    assert_eq!(
        (&moves[0]["from"], &moves[0]["to"]),
        (&json!("canned-rot:a"), &json!("canned-rot:b"))
    );
    assert!(
        moves[0]["cause"]["Auth"]["line"]
            .as_str()
            .is_some_and(|l| l.contains("401")),
        "{moves}"
    );
    assert!(ev.requests.iter().any(|r| presented(r, KEY_B)));
    assert_no_key_kept(&ev);
}

/// **A root rotates as a child does**, through the same policy (`run::relaunch_on`): a codex root on
/// `canned-rot` whose provider answers key `a` with 429 before any turn is relaunched fresh on `b`
/// — the same node, a second `Spawned` naming the credential by id — and finishes there. The key
/// is in no file the tree keeps.
#[test]
fn a_rate_limited_root_fails_over_to_the_next_stated_credential_before_its_first_turn() {
    use marion_supervisor::root::{self, RootSpec};
    assert!(
        on_path("codex"),
        "put `codex` ({}) on PATH",
        pinned_version("codex")
    );
    let reqlog_dir = scratch("endpoint-reqlog-root-rot");
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: reqlog_dir.join("provider-requests.jsonl"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            refusals: vec![marion_provider::KeyRefusal {
                key: KEY_A.into(),
                status: 429,
            }],
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let base_url = server.base_url();
    let _cells = providers_at(&base_url);
    let t = tree("root-rot", Some(base_url.clone()));
    let node = root::prepare(&RootSpec {
        agent_type: "codex".into(),
        prompt: "Report back through marion.".into(),
        native_launch: None,
        repo: t.repo.canonicalize().unwrap(),
        state: t.state.clone(),
        base_url: Some(base_url),
        auth: marion_harness::Auth::Canned,
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        model: Some(format!("canned-rot:{MODEL}")),
        no_change_record: false,
        pane: false,
        resume: None,
        bound_secs: 120,
        profile: None,
    })
    .expect("the endpoint root prepares");
    assert!(
        node.rotation.is_some(),
        "a headless root with a second credential can rotate"
    );
    let outcome = root::launch(
        &node,
        std::time::Duration::from_secs(120),
        root::MCP_READY_TIMEOUT,
    );
    let requests = server.requests().unwrap_or_default();
    // The root's own project key: `prepare` keys it on the canonical repository root.
    let journal = std::fs::read_to_string(node.project.journal()).unwrap_or_default();
    let mut kept = Vec::new();
    every_file(&t.state, &mut kept);
    drop(node);
    drop(server);
    let leaked = survivors(&t.state.parent().unwrap().to_string_lossy());
    for (pid, _) in &leaked {
        kill_hard(*pid);
    }
    let summary = requests
        .iter()
        .map(|r| format!("  {} {} {}", r["method"], r["path"], r["credentials"]))
        .collect::<Vec<_>>()
        .join("\n");
    let outcome = outcome.unwrap_or_else(|e| panic!("the root ran: {e}\n{summary}"));
    assert_eq!(
        outcome.exit_code,
        Some(0),
        "the root finished on b: {:?} {}\n{summary}",
        outcome.failure,
        outcome.stderr
    );
    let first_b = requests
        .iter()
        .position(|r| presented(r, KEY_B))
        .unwrap_or_else(|| panic!("key b was never presented\n{summary}"));
    assert!(
        first_b > 0 && presented(&requests[0], KEY_A),
        "a first\n{summary}"
    );
    assert!(
        requests[first_b..].iter().all(|r| presented(r, KEY_B)),
        "after the failover only b is presented\n{summary}"
    );
    // One node, two process lifetimes, each naming the credential it ran on.
    let a = journal.find("\"credential\":\"canned-rot:a\"");
    let b = journal.find("\"credential\":\"canned-rot:b\"");
    assert!(
        matches!((a, b), (Some(a), Some(b)) if a < b),
        "the journal's Spawned records name a, then b:\n{journal}"
    );
    for key in [KEY_A, KEY_B] {
        let holding: Vec<&PathBuf> = kept
            .iter()
            .filter(|(_, bytes)| bytes.windows(key.len()).any(|w| w == key.as_bytes()))
            .map(|(p, _)| p)
            .collect();
        assert!(holding.is_empty(), "a key is kept in {holding:?}");
    }
    assert!(leaked.is_empty(), "leaked {leaked:?}");
}

// ---- `marion doctor --providers`: each stored credential, probed; the harness x provider matrix --

/// **Each stored credential is checked by id, and the key is never shown**: present or not, its
/// provider reachable through `GET <base>/models` with the key in the header the provider reads,
/// the requested model listed or not — and a key the provider refuses reported as refused.
#[test]
fn doctor_providers_checks_each_stored_credential_against_its_endpoint() {
    use marion_supervisor::provider_check::{self, Reach};
    let reqlog_dir = scratch("endpoint-reqlog-doctor");
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: reqlog_dir.join("provider-requests.jsonl"),
        script: Script {
            models: vec![MODEL.into(), "other-model".into()],
            refusals: vec![marion_provider::KeyRefusal {
                key: KEY_A.into(),
                status: 401,
            }],
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let _cells = providers_at(&server.base_url());
    let rows = provider_check::check_user(Some(MODEL)).expect("the user's config reads");
    let row = |id: &str| {
        rows.iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("no row for {id}: {rows:#?}"))
    };
    let ok = row("canned-test");
    assert!(ok.key_present);
    assert_eq!(
        ok.reach,
        Some(Reach::Listed {
            models: 2,
            model_listed: Some(true)
        }),
        "{ok:#?}"
    );
    assert_eq!(row("canned-rot:a").reach, Some(Reach::Refused(401)));
    let nokey = row("canned-nokey");
    assert!(!nokey.key_present);
    assert_eq!(nokey.reach, None, "no key, no probe");
    #[cfg(target_os = "linux")]
    assert_eq!(ok.file_mode, Some(0o600));
    // The x-api-key provider was asked in its own header.
    let requests = server.requests().unwrap_or_default();
    assert!(
        requests.is_empty(),
        "a GET is not logged as a model request"
    );
    let text = provider_check::render(
        &rows,
        &provider_check::matrix(&marion_supervisor::credentials::user_registry().unwrap()),
    );
    for key in [KEY, KEY_A, KEY_B] {
        assert!(!text.contains(key), "the key is in doctor's output");
    }
    assert!(
        text.contains("canned-test") && text.contains("listed"),
        "{text}"
    );
}

/// **The matrix is the resolver's answer, harness by provider**: native on the first shared wire,
/// translated where none is shared but the gateway bridges the pair, unsupported with both wire
/// lists where neither holds, and unsupported naming the header where
/// the recipe cannot present the provider's key.
#[test]
fn doctor_providers_matrix_is_computed_by_the_endpoint_resolver() {
    use marion_core::harness::Harness;
    use marion_supervisor::provider_check::{Cell, matrix};
    let _cells = providers_at("http://127.0.0.1:9/v1");
    let reg = marion_supervisor::credentials::user_registry().unwrap();
    let m = matrix(&reg);
    let cell = |h: Harness, p: &str| {
        m.iter()
            .find(|c| c.harness == h && c.provider == p)
            .map(|c| c.cell.clone())
            .unwrap_or_else(|| panic!("no cell {h} {p}"))
    };
    assert_eq!(
        cell(Harness::Codex, "openai"),
        Cell::Native("openai-responses".into())
    );
    assert_eq!(
        cell(Harness::Copilot, "canned-anthropic"),
        Cell::Native("anthropic".into())
    );
    // No shared wire, but one marion's gateway translates: Claude Code on a Chat-only provider.
    assert_eq!(
        cell(Harness::ClaudeCode, "canned-chat"),
        Cell::Translated("anthropic>openai-chat".into())
    );
    // And a Chat-only harness on an Anthropic-only provider.
    assert_eq!(
        cell(Harness::OpenCode, "canned-anthropic"),
        Cell::Translated("openai-chat>anthropic".into())
    );
    match cell(Harness::Codex, "groq") {
        Cell::Unsupported(why) => assert!(
            why.contains("openai-responses") && why.contains("openai-chat"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    match cell(Harness::Codex, "canned-chat-xkey") {
        Cell::Unsupported(why) => assert!(why.contains("openai-chat"), "{why}"),
        other => panic!("{other:?}"),
    }
    match cell(Harness::Goose, "canned-chat-xkey") {
        Cell::Unsupported(why) => assert!(why.contains("x-api-key"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        cell(Harness::OpenCode, "canned-chat-xkey"),
        Cell::Native("openai-chat".into())
    );
    match cell(Harness::Acp, "openai") {
        Cell::Unsupported(why) => assert!(why.contains("no endpoint wire"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// **`marion-supervisor doctor --providers` prints the report and never a key**, reading the same
/// user configuration a launch reads.
#[test]
fn doctor_providers_on_the_command_line_prints_ids_and_the_matrix_and_no_key() {
    let reqlog_dir = scratch("endpoint-reqlog-doctor-cli");
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: reqlog_dir.join("provider-requests.jsonl"),
        script: Script {
            models: vec![MODEL.into()],
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let _cells = providers_at(&server.base_url());
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_marion-supervisor"))
        .args(["doctor", "--providers", "--model", MODEL])
        .output()
        .expect("the supervisor runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("canned-test: key stored") && stdout.contains("requested model listed"),
        "{stdout}"
    );
    assert!(stdout.contains("codex=openai-responses"), "{stdout}");
    for key in [KEY, KEY_A, KEY_B] {
        assert!(!stdout.contains(key), "the key is in doctor's output");
    }
}

// ---- the remaining rows with an endpoint recipe: gemini, goose, cline, qwen ----------------------

/// A cell of `agent_type` on `canned-test` over one of the OpenAI-compatible Chat recipes, with
/// marion's report under the harness's own tool spelling.
fn chat_cell(agent_type: &'static str, report_tool: &str) -> Cell {
    Cell {
        presents: Presents::Bearer,
        provider: "canned-test",
        agent_type,
        script: Script {
            openai_report_tool: report_tool.into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "openai",
        route: "native",
    }
}

#[test]
fn a_gemini_child_runs_on_the_users_provider_over_the_gemini_wire() {
    assert!(
        on_path("gemini"),
        "put `gemini` ({}) on PATH",
        pinned_version("gemini")
    );
    let cell = Cell {
        presents: Presents::GoogApiKey,
        provider: "canned-test",
        agent_type: "gemini",
        script: Script {
            gemini_report_tool: "mcp_marion_report".into(),
            gemini_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        compiled_model: MODEL,
        wire: "gemini",
        route: "native",
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}

#[test]
fn a_goose_child_runs_on_the_users_provider_over_the_chat_wire() {
    assert!(
        on_path("goose"),
        "put `goose` ({}) on PATH",
        pinned_version("goose")
    );
    let cell = chat_cell("goose", "marion__report");
    assert_endpoint_cell(&cell, &drive(&cell));
}

#[test]
fn a_cline_child_runs_on_the_users_provider_over_the_chat_wire() {
    assert!(
        on_path("cline"),
        "put `cline` ({}) on PATH",
        pinned_version("cline")
    );
    let cell = chat_cell("cline", "marion__report");
    assert_endpoint_cell(&cell, &drive(&cell));
}

#[test]
fn a_qwen_child_runs_on_the_users_provider_over_the_chat_wire() {
    assert!(
        on_path("qwen"),
        "put `qwen` ({}) on PATH",
        pinned_version("qwen")
    );
    let cell = chat_cell("qwen", "mcp__marion__report");
    assert_endpoint_cell(&cell, &drive(&cell));
}
