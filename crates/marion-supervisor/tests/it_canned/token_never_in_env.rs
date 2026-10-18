//! **No node token value is ever in a process environment** — not the harness's, and not the
//! bridge's the harness starts.
//!
//! Any process running as the operator reads the environment of any other (`KERN_PROCARGS2` on
//! macOS, `/proc/<pid>/environ` on Linux), a sandboxed node's included, so a token in an
//! environment is a token every node holds. marion writes it to a 0600 file in the node's own
//! directory and lets only that file's path travel (`marion_harness::spec::TokenCarrier`).
//!
//! Asserted over every built-in agent type and every ACP agent, in canned and live mode, through
//! the supervisor's own `root::prepare_watched` with an owner that mints the token:
//!
//! * the harness's environment, as [`Invocation::command`] really builds it, read back from a
//!   started process (the row's program swapped for `env`, so no harness runs);
//! * the bridge's, started from the declaration the row compiled the way its harness starts one,
//!   read back from the running process's own environment;
//! * every file marion wrote for the node but the token's own, which holds it 0600.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use marion_core::contract::AgentId;
use marion_core::secret::Secret;
use marion_harness::Auth;
use marion_harness::mcp_bridge::{NODE_TOKEN_ENV, NODE_TOKEN_FILE, NODE_TOKEN_FILE_ENV};
use marion_supervisor::root::{RootNode, RootSpec, prepare_watched};
use marion_testsupport::{fixture_repo, scratch};

// Distinctive enough that it cannot collide with an id, a path or a model name.
const TOKEN: &str = "inv-node-token-5d2e-41c7-9b0a-c3f8e6a17d40";

struct Owner;

impl marion_supervisor::run::SpawnObserver for Owner {
    fn identified(&self, _: &AgentId) -> Option<Secret> {
        Some(TOKEN.into())
    }
    fn started(&self, _: &AgentId, _: i32) {}
}

/// Every agent type a root can be: the built-ins, and one per ACP agent.
fn agent_types() -> Vec<String> {
    marion_core::agent_type::builtin_names()
        .iter()
        .map(|n| n.to_string())
        .chain(
            marion_harness::acp::AGENTS
                .iter()
                .map(|a| format!("acp:{}", a.id)),
        )
        .collect()
}

fn root_spec(dir: &Path, agent_type: &str, auth: Auth) -> RootSpec {
    RootSpec {
        os_sandbox: true,
        wider_children: false,
        budget: None,
        agent_type: agent_type.into(),
        prompt: "delegate it".into(),
        native_launch: None,
        repo: dir.join("repo"),
        state: dir.join("state"),
        base_url: (auth == Auth::Canned).then(|| "http://127.0.0.1:8099/v1".into()),
        bridge: env!("CARGO_BIN_EXE_marion-supervisor").into(),
        // A live node keeps its harness's own default: a built-in's names the canned provider.
        model: match auth {
            Auth::Canned => marion_core::agent_type::builtin(agent_type).and_then(|t| t.model),
            _ => None,
        },
        auth,
        no_change_record: false,
        pane: false,
        resume: None,
        bound_secs: marion_core::agent_type::DEFAULT_TIMEOUT_SECS,
        profile: None,
    }
}

/// The environment the harness process really starts with: the node's own invocation, its
/// program swapped for `env` and run unsandboxed, so what prints is what `command` hands it.
fn harness_env(node: &RootNode, tmp: &Path) -> String {
    let mut probe = node.invocation.clone();
    probe.program = "/usr/bin/env".into();
    probe.args = Vec::new();
    probe.sandbox = None;
    let out = probe
        .command(tmp)
        .stdin(Stdio::null())
        .output()
        .expect("`env` runs");
    assert!(out.status.success(), "`env` exited {}", out.status);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Every `MARION_*` pair the node's declaration states, wherever its route put it — its
/// document, its argv, its environment or its `session/new` request — in each spelling a pair
/// takes across them
/// (`"K": "v"`, `K = "v"`, `K="v"`, `K=v`).
fn declared_pairs(node: &RootNode) -> BTreeMap<String, String> {
    let mut text: Vec<String> = node.invocation.args.clone();
    text.extend(node.invocation.env.iter().map(|(k, v)| format!("{k}={v}")));
    if let Some(doc) = node
        .mcp_config
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        text.push(doc);
    }
    // ACP's `session/new` spells a pair as `{"name": K, "value": v}`; respelled `K="v"`.
    if let Some(session) = &node.session_declaration {
        text.push(session.to_string());
        let mut stack = vec![session];
        while let Some(v) = stack.pop() {
            if let (Some(k), Some(val)) = (
                v.get("name").and_then(|k| k.as_str()),
                v.get("value").and_then(|v| v.as_str()),
            ) {
                text.push(format!("{k}=\"{val}\""));
            }
            match v {
                serde_json::Value::Array(a) => stack.extend(a),
                serde_json::Value::Object(o) => stack.extend(o.values()),
                _ => {}
            }
        }
    }
    let mut out = BTreeMap::new();
    for t in &text {
        for (at, _) in t.match_indices("MARION_") {
            let rest = &t[at..];
            let len = rest
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(rest.len());
            let (key, rest) = rest.split_at(len);
            let rest = rest.trim_start_matches('"').trim_start();
            let Some(rest) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('=')) else {
                continue;
            };
            let rest = rest.trim_start();
            let value = match rest.strip_prefix('"') {
                Some(quoted) => quoted.split('"').next().unwrap_or_default(),
                None => rest
                    .split(|c: char| c.is_whitespace() || c == ',' || c == '}')
                    .next()
                    .unwrap_or_default(),
            };
            if !value.is_empty() {
                out.insert(key.to_string(), value.to_string());
            }
        }
    }
    out
}

/// `pid`'s own environment, as the kernel holds it.
fn process_env(pid: u32) -> String {
    if cfg!(target_os = "linux") {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).expect("/proc/<pid>/environ");
        String::from_utf8_lossy(&raw).replace('\0', "\n")
    } else {
        let out = Command::new("ps")
            .args(["-Eww", "-o", "command=", "-p", &pid.to_string()])
            .output()
            .expect("`ps -E` runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// The bridge, started from the declaration as the node's harness starts it — the declared pairs
/// over the harness's own environment — and its environment read while it serves.
fn bridge_env(declared: &BTreeMap<String, String>, harness_env: &str) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion-supervisor"));
    cmd.arg("mcp").env_clear();
    for line in harness_env.lines() {
        if let Some((k, v)) = line.split_once('=') {
            cmd.env(k, v);
        }
    }
    cmd.envs(declared)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("the bridge starts");
    let env = process_env(child.id());
    let _ = child.kill();
    let _ = child.wait();
    env
}

/// The `MARION_NODE_TOKEN_FILE` an environment's text names, if any.
fn named_file(env: &str) -> Option<String> {
    let needle = format!("{NODE_TOKEN_FILE_ENV}=");
    let at = env.rfind(&needle)?;
    Some(
        env[at + needle.len()..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect(),
    )
}

#[test]
fn no_harness_or_bridge_environment_holds_a_node_token_on_any_row_in_either_mode() {
    let dir = scratch("token-never-in-env");
    fixture_repo(&dir);
    std::fs::create_dir_all(dir.join("state")).unwrap();
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut covered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut held_refused: Vec<marion_core::Harness> = Vec::new();
    for agent_type in agent_types() {
        for auth in [Auth::Canned, Auth::Inherited] {
            let who = format!("{agent_type} {auth:?}");
            // A launch the row refuses (no canned route, a held row) starts nothing, so no
            // environment can hold its token. A held row's refusal is recorded, in its own words.
            let node = match prepare_watched(&root_spec(&dir, &agent_type, auth), &Owner) {
                Ok(node) => node,
                Err(e) => {
                    let e = e.to_string();
                    held_refused.extend(marion_core::Harness::ALL.into_iter().filter(|h| {
                        marion_harness::adapter::harness_spec(*h)
                            .held
                            .is_some_and(|why| e.contains(why))
                    }));
                    continue;
                }
            };
            let token_file = node.agent_dir.config_dir().join(NODE_TOKEN_FILE);
            let path = token_file.to_string_lossy().into_owned();

            // The token's one file: present, the token, and the operator's alone (written through
            // `AgentFs`, as every document of the launch is).
            assert_eq!(
                std::fs::read_to_string(&token_file).ok().as_deref(),
                Some(TOKEN),
                "{who}: the minted token is in its file"
            );
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{who}: {path} is private");
            }

            // The harness's argv, which every user reads through `ps`.
            assert!(
                node.invocation.args.iter().all(|a| !a.contains(TOKEN)),
                "{who}: the node token is on the harness's argv"
            );

            // The harness's environment.
            let harness = harness_env(&node, &tmp);
            assert!(
                !harness.contains(TOKEN),
                "{who}: the node token is in the harness's environment"
            );
            assert!(
                !harness.contains(&format!("{NODE_TOKEN_ENV}=")),
                "{who}: the harness's environment sets {NODE_TOKEN_ENV}"
            );
            if let Some(named) = named_file(&harness) {
                assert_eq!(named, path, "{who}: the harness names the token's own file");
            }

            // The declaration, wherever it rides.
            let declared = declared_pairs(&node);
            assert!(
                declared.values().all(|v| !v.contains(TOKEN)),
                "{who}: the declaration carries the token: {declared:?}"
            );
            assert!(
                !declared.contains_key(NODE_TOKEN_ENV),
                "{who}: the declaration sets {NODE_TOKEN_ENV}: {declared:?}"
            );

            // The bridge the harness would start from it, which must still find the file.
            let bridge = bridge_env(&declared, &harness);
            assert!(
                !bridge.contains(TOKEN),
                "{who}: the node token is in the bridge's environment"
            );
            assert_eq!(
                named_file(&bridge).as_deref(),
                Some(path.as_str()),
                "{who}: the bridge is pointed at the token's file"
            );

            // Every other file marion wrote for the node.
            crate::common::walk(node.agent_dir.path(), &mut |p: &Path| {
                if p == token_file {
                    return;
                }
                let bytes = std::fs::read(p).unwrap_or_default();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(TOKEN),
                    "{who}: the node token escaped its file into {}",
                    p.display()
                );
            });
            covered
                .entry(format!("{:?}", node.harness))
                .or_default()
                .push(format!("{auth:?}"));
        }
    }
    // Every row live, and canned too wherever its canned carrier is a different one, so no
    // carrier goes unchecked. A held row is refused instead, which is what keeps any environment
    // from holding its token; its carriers are swept with the hold lifted in marion-harness
    // (`no_launch_puts_the_node_token_on_argv_and_every_launch_delivers_it`).
    for h in marion_core::Harness::ALL {
        let row = marion_harness::adapter::harness_spec(h);
        if row.held.is_some() {
            assert!(
                held_refused.contains(&h),
                "{h:?}: a held row was neither prepared nor refused as held: {covered:?}"
            );
            continue;
        }
        let carriers = row.token;
        let mut modes = vec!["Inherited"];
        if carriers.canned != carriers.live {
            modes.push("Canned");
        }
        for auth in modes {
            assert!(
                covered
                    .get(&format!("{h:?}"))
                    .is_some_and(|m| m.iter().any(|a| a == auth)),
                "{h:?} {auth}: never prepared, so its carrier went unchecked: {covered:?}"
            );
        }
    }
}
