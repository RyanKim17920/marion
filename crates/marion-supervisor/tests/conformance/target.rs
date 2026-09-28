//! **What the battery runs against: every row marion has**, and each launch compiled the way a
//! marion spawn compiles it.
//!
//! The target list is `Harness::ALL`, with the ACP row expanded over `acp::AGENTS` exactly as
//! `marion doctor` expands it — so a new row, or a new ACP refinement, is in the battery the day it
//! lands and nothing here names it. Each target's agent type is the built-in marion would spawn
//! for that harness (the plain implementer), and each launch goes through
//! `run::child_launch_spec` → `config_files` → `compile` → `session_declaration`, the one
//! derivation a real child takes; the battery adds nothing to the launch but the provider URL.

use std::path::{Path, PathBuf};

use marion_core::agent_type::{AgentType, builtin, builtin_names};
use marion_core::contract::Isolation;
use marion_core::harness::Harness;
use marion_core::paths::ProjectDir;
use marion_harness::adapter::harness_spec;
use marion_harness::invocation::Invocation;
use marion_harness::spec::{HarnessSpec, Remembers};
use marion_harness::{Auth, HarnessAdapter, SpawnCtx, acp, adapter_for_type};
use marion_supervisor::duplex::{LaunchPath, launch_path};
use marion_supervisor::run::{Env, SpawnRequest, child_launch_spec, child_prompt};
use serde_json::Value;

pub struct Target {
    /// `claude-code`, `opencode`, …, `acp:opencode` — the matrix row's name.
    pub selector: String,
    pub agent_type: AgentType,
    pub adapter: Box<dyn HarnessAdapter + Send + Sync>,
    pub spec: &'static HarnessSpec,
    pub path: LaunchPath,
}

impl Target {
    /// marion's `report` as this target's model spells it.
    pub fn report_name(&self) -> String {
        self.adapter.marion_tool_name("report")
    }

    /// The program the row launches: its own, or the ACP agent's first argv word.
    pub fn program(&self) -> Option<String> {
        match self.acp_agent() {
            Some(a) => a.argv.first().map(|s| s.to_string()),
            None => self.spec.program.map(str::to_string),
        }
    }

    pub fn acp_agent(&self) -> Option<acp::Agent> {
        self.agent_type.acp_agent.as_deref().and_then(acp::agent)
    }
}

/// Every row, as `(selector, target or the reason none could be built)`.
pub fn all() -> Vec<(String, Result<Target, String>)> {
    let mut out = Vec::new();
    for h in Harness::ALL {
        if h == Harness::Acp {
            for a in acp::AGENTS {
                let selector = format!("acp:{}", a.id);
                out.push((selector.clone(), build(selector, h, acp_type(a.id))));
            }
        } else {
            let selector = h.as_str().to_string();
            out.push((selector.clone(), build(selector, h, plain_type(h))));
        }
    }
    out
}

/// The built-in marion spawns for a harness when asked for it by name: the first built-in on that
/// harness that is not an orchestrator.
fn plain_type(h: Harness) -> Option<AgentType> {
    builtin_names()
        .iter()
        .filter(|n| !n.ends_with("-orchestrator"))
        .filter_map(|n| builtin(n))
        .find(|t| t.harness == h && t.acp_agent.is_none())
}

/// A built-in bound to this ACP agent, or the open-ended `acp:<id>` type every agent has.
fn acp_type(id: &str) -> Option<AgentType> {
    builtin_names()
        .iter()
        .filter_map(|n| builtin(n))
        .find(|t| t.harness == Harness::Acp && t.acp_agent.as_deref() == Some(id))
        .or_else(|| builtin(&format!("acp:{id}")))
}

fn build(selector: String, h: Harness, ty: Option<AgentType>) -> Result<Target, String> {
    let agent_type = ty.ok_or_else(|| format!("no built-in agent type runs on {h}"))?;
    let adapter = adapter_for_type(h, agent_type.acp_agent.as_deref())
        .map_err(|e| format!("adapter: {e}"))?;
    let path = launch_path(&adapter.surfaces())
        .ok_or_else(|| format!("{h} declares no surface marion drives a node over"))?;
    Ok(Target {
        selector,
        agent_type,
        adapter,
        spec: harness_spec(h),
        path,
    })
}

/// One probe's world: a scratch tree with a repo, a state dir and the node's config dir.
pub struct World {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub state: PathBuf,
    pub config: PathBuf,
}

impl World {
    pub fn new(root: &Path, name: &str) -> Self {
        let root = root.join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("probe world");
        let repo = marion_testsupport::fixture_repo(&root);
        let state = root.join("state");
        std::fs::create_dir_all(&state).expect("state dir");
        let config = root.join("node");
        std::fs::create_dir_all(&config).expect("config dir");
        Self {
            root,
            repo,
            state,
            config,
        }
    }
}

/// The real bridge, as every marion spawn declares it.
pub fn bridge() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor"))
}

/// A launch as marion compiles it, and what the probe needs to drive it.
pub struct Launch {
    pub inv: Invocation,
    /// The documents the adapter decided on, already written.
    pub files: Vec<PathBuf>,
    /// ACP's `session/new` (or `session/load`), with marion's declaration in it.
    pub session: Option<Value>,
    /// The file marion's bridge writes once MCP is up (the duplex gate), where the path has one.
    pub ready_file: Option<PathBuf>,
}

/// What a probe may change in a launch before it is compiled.
#[derive(Default)]
pub struct Knobs {
    pub resume: Option<String>,
    /// The bridge program declared to the harness, in place of the real one (the readiness probe's
    /// slow bridge).
    pub bridge: Option<PathBuf>,
    /// Compile the row's pane shape (a TUI opened at its composer) instead of the headless one.
    pub pane: bool,
}

pub fn compile(
    t: &Target,
    w: &World,
    base_url: &str,
    prompt: &str,
    knobs: &Knobs,
) -> Result<Launch, String> {
    let bridge = knobs.bridge.clone().unwrap_or_else(bridge);
    let env = Env {
        project_dir: ProjectDir::new(&w.state, &w.repo),
        state: w.state.clone(),
        project_root: w.repo.clone(),
        bridge: bridge.clone(),
        base_url: Some(base_url.to_string()),
        auth: Auth::Canned,
    };
    let req = SpawnRequest {
        agent_type: t.agent_type.name.clone(),
        prompt: child_prompt(&t.agent_type, prompt),
        repo: w.repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["**".into()],
        timeout_secs: 120,
        model: None,
        isolation: Isolation::SharedCwd,
        allow_concurrent_writes: true,
        resume: None,
        // A canned launch runs on no profile (`profiles::Launch::resolve` refuses every
        // non-inherited auth).
        profile: None,
    };
    let mut spec = child_launch_spec(
        &env,
        &req,
        &t.agent_type,
        t.adapter.as_ref(),
        if knobs.pane {
            LaunchPath::Terminal
        } else {
            t.path
        },
        // A child of a root, the one depth every probe drives.
        1,
        &w.repo,
        &w.config,
    );
    spec.resume = knobs.resume.clone();
    if knobs.pane {
        // A pane opens at its composer; the probe types the prompt itself.
        spec.prompt = String::new();
    }
    // The duplex gate's marker. A pane gets one too: an adapter whose headless prompt is written
    // after launch refuses to declare its bridge without it, and on a pane nobody waits on it.
    let ready_file =
        (knobs.pane || matches!(t.path, LaunchPath::Duplex)).then(|| w.config.join("mcp-ready"));
    if let Some(f) = &ready_file {
        let _ = std::fs::remove_file(f);
    }
    let ctx = SpawnCtx {
        agent_id: marion_core::contract::AgentId("conformance-node".into()),
        agent_type: t.agent_type.name.clone(),
        depth: 1,
        node_token: None,
        ready_file: ready_file.clone(),
        repo: w.repo.clone(),
        state_dir: w.state.clone(),
        bridge,
        bridge_args: vec!["mcp".into()],
    };
    let files = t
        .adapter
        .config_files(&spec, &ctx)
        .map_err(|e| format!("config_files: {e}"))?;
    let mut written = Vec::new();
    for (path, body) in files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
        written.push(path);
    }
    let mut inv = if knobs.pane {
        t.adapter.compile_pane(&spec, &ctx)
    } else {
        t.adapter.compile(&spec, &ctx)
    }
    .map_err(|e| format!("compile: {e}"))?;
    // A pane is where a boot dialog gets answered. A row whose TUI keeps that answer in the
    // operator's own config (`Remembers::OperatorConfig`) runs on a config moved into this world,
    // still on the operator's login, so the answer is left in the scratch and never in theirs.
    if knobs.pane
        && let Remembers::OperatorConfig(r) = t.spec.boot_dialogs.remembers
    {
        let moved = r
            .apply(&w.root.join("harness-config"), |k| std::env::var(k).ok())
            .map_err(|e| format!("moving {} into the world: {e}", r.config))?;
        inv.env.retain(|(k, _)| !moved.iter().any(|(m, _)| m == k));
        inv.env.extend(moved);
    }
    let session = t
        .adapter
        .session_declaration(&spec, &ctx)
        .map_err(|e| format!("session_declaration: {e}"))?;
    t.adapter
        .mcp_route(&spec)
        .verify(&written, &inv, session.as_ref())
        .map_err(|e| format!("the compiled launch does not carry marion's declaration: {e}"))?;
    Ok(Launch {
        inv,
        files: written,
        session,
        ready_file,
    })
}
