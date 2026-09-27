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
use marion_core::paths::ProjectDir;
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, scratch};

/// The fixture key. The test writes it; nothing in marion ever mints it.
const KEY: &str = "sk-endpoint-test";

struct Fixture {
    config: PathBuf,
}

/// The one fixture config dir for this binary, with the environment pointed at it.
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("marion-endpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config = root.join("marion");
        std::fs::create_dir_all(&config).unwrap();
        write_owner_only(
            &config.join("credentials.json"),
            &format!(
                "{{\"providers\": {{\"canned-test\": \"{KEY}\", \"canned-anthropic\": \"{KEY}\", \
                 \"canned-anthropic-bearer\": \"{KEY}\", \"canned-chat-xkey\": \"{KEY}\"}}}}\n"
            ),
        );
        // SAFETY: set once, inside `get_or_init`, before any test in this binary has read the
        // environment or started a process — every test's first act is to call this.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &root);
            std::env::set_var("MARION_CREDENTIAL_STORE", "file");
        }
        Fixture { config }
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
/// with no stored key, `canned-chat-xkey` serving Chat Completions and reading `x-api-key`.
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
             key_header = \"x-api-key\"\n",
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
    let env = Env {
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state: state.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url,
        auth: marion_harness::Auth::Canned,
    };
    Tree {
        _root: root,
        repo,
        state,
        env,
    }
}

fn request(t: &Tree, agent_type: &str, model: &str) -> SpawnRequest {
    SpawnRequest {
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

/// The model every cell asks the provider for, through the `canned-test:` prefix.
const MODEL: &str = "endpoint-model-7";
const NARRATIVE: &str = "Reported back through marion from an endpoint node.";

/// How the harness presents the key: `Authorization: Bearer <key>`, or the Anthropic SDK's
/// `x-api-key: <key>`.
#[derive(Clone, Copy)]
enum Presents {
    Bearer,
    XApiKey,
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
}

struct Evidence {
    contract: Result<TaskContract, String>,
    persisted: Vec<Value>,
    requests: Vec<Value>,
    journal: String,
    leaked: Vec<String>,
}

fn drive(cell: &Cell) -> Evidence {
    let reqlog_dir = scratch(&format!("endpoint-reqlog-{}", cell.agent_type));
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: reqlog_dir.join("provider-requests.jsonl"),
        script: cell.script.clone(),
    })
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
    Evidence {
        contract,
        persisted,
        requests,
        journal,
        leaked: leaked.into_iter().map(|(_, l)| l).collect(),
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
        // No request names any model but the chosen one — background calls included.
        if let Some(m) = r["body"]["model"].as_str() {
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
        ev.requests.iter().any(|r| r["body"]["model"] == MODEL),
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
    assert_eq!(contract.child.route.as_deref(), Some("native"), "{who}");
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
    };
    assert_endpoint_cell(&cell, &drive(&cell));
}
