//! **Desktop notifications**: the supervisor tells the operator when a node needs them, finished,
//! failed or hit a limit — opt-in, off unless `notify.toml` or `MARION_NOTIFY` turns it on.
//!
//! # What a notice may say
//!
//! Only enumerated fields: the kind, the node's short id, its agent type, its harness and its
//! state word ([`Notice`]). Never a prompt, a narrative, a tool's arguments or a failure line —
//! those are the operator's and the model's words, and a notification is shown on a lock screen.
//!
//! # When
//!
//! Driven by the registry: [`Notifier::observe`] runs from the supervisor's flush, returns at once
//! unless the journal moved, and reports each node's transition into a kind once. The first
//! observation only learns what is already true, so a restarted supervisor replays nothing. No
//! timer: a notice is a consequence of a journal record.
//!
//! # Where
//!
//! One [`Backend`], resolved once: `osascript` on macOS, `notify-send` where it is on `PATH`, else
//! the terminal of the client that claimed it (`notify/claim`, a BEL or an OSC between frames), or
//! a file for the tests (`MARION_NOTIFY_BACKEND=record:<path>`). A process backend runs on one
//! delivery thread blocked on its channel, started at the first notice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use marion_core::contract::{AgentId, ExitStatus};
use marion_core::journal::CancelBy;
use marion_core::node::{NodeState, ReapState};
use marion_core::registry::{Replay, ReplayedNode};
use serde::{Deserialize, Serialize};

/// The environment switch: `on` or `off`, over the file.
pub const NOTIFY_ENV: &str = "MARION_NOTIFY";
/// The backend override: `record:<path>`, `terminal`, `off`, `osascript`, `notify-send`.
pub const BACKEND_ENV: &str = "MARION_NOTIFY_BACKEND";
/// The file under marion's config dir.
pub const CONFIG_FILE: &str = "notify.toml";
/// More notices than this in one flush are merged into one.
const MERGE_ABOVE: usize = 3;

/// What happened, in the four words a notice can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NoticeKind {
    /// Blocked on the operator: a permission, a boot dialog, a hold.
    NeedsYou,
    /// Ended `Ok`.
    Finished,
    /// Ended failed, unreported or killed, or orphaned.
    Failed,
    /// Ended on a limit: its wall clock, or its token budget.
    Limit,
}

impl NoticeKind {
    fn words(self) -> &'static str {
        match self {
            NoticeKind::NeedsYou => "needs you",
            NoticeKind::Finished => "finished",
            NoticeKind::Failed => "failed",
            NoticeKind::Limit => "hit a limit",
        }
    }
}

/// **The kind of notice a node in this state warrants, if any** — a total function of the node's
/// state, reap state and cancel. A cancel is a decision somebody made, so it tells nobody, unless a
/// budget made it.
pub fn kind_of(node: &ReplayedNode) -> Option<NoticeKind> {
    let budget = matches!(
        node.cancel.as_ref().map(|c| &c.by),
        Some(CancelBy::Budget { .. })
    );
    match (node.state, node.reap_state) {
        (NodeState::Exited(ExitStatus::Ok), _) => Some(NoticeKind::Finished),
        (NodeState::Exited(ExitStatus::TimedOut), _) => Some(NoticeKind::Limit),
        (NodeState::Exited(ExitStatus::Cancelled), _) if budget => Some(NoticeKind::Limit),
        (NodeState::Exited(ExitStatus::Cancelled), _) => None,
        (
            NodeState::Exited(ExitStatus::Failed | ExitStatus::Unreported | ExitStatus::Killed),
            _,
        ) => Some(NoticeKind::Failed),
        (_, ReapState::Orphaned) => Some(NoticeKind::Failed),
        (_, ReapState::ReapedIdle) => None,
        (NodeState::Blocked(_), ReapState::Live) => Some(NoticeKind::NeedsYou),
        (
            NodeState::Spawning | NodeState::Ready | NodeState::Running | NodeState::Idle,
            ReapState::Live,
        ) => None,
    }
}

/// One notice, of enumerated fields only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub kind: NoticeKind,
    pub short: String,
    pub agent_type: String,
    pub harness: String,
    pub state: String,
}

impl Notice {
    fn of(node: &ReplayedNode, kind: NoticeKind) -> Notice {
        Notice {
            kind,
            short: crate::tree::short_id(&node.agent_id.0).to_string(),
            agent_type: node.agent_type().unwrap_or("node").to_string(),
            harness: node
                .harness()
                .map_or("unknown", marion_core::harness::Harness::cli_name)
                .to_string(),
            state: crate::tree::state_label(node.state, node.reap_state),
        }
    }

    /// The body: `codex-impl 8ea3 (codex) failed — exited:failed`.
    pub fn body(&self) -> String {
        format!(
            "{} {} ({}) {} — {}",
            self.agent_type,
            self.short,
            self.harness,
            self.kind.words(),
            self.state
        )
    }
}

/// What a backend is handed: the title and the body, both built from enumerated fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shown {
    pub title: String,
    pub body: String,
}

/// Which finished nodes are worth a notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Finished {
    /// Only a root finishing: the run is done.
    #[default]
    Roots,
    /// Every node.
    All,
    /// None; failures and needs still notify.
    Off,
}

/// How a terminal client rings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalRing {
    /// BEL.
    #[default]
    Bell,
    /// OSC 9, the notification escape iTerm2, kitty and others show.
    Osc9,
    /// OSC 777, rxvt's and foot's.
    Osc777,
}

/// `notify.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotifyConfig {
    /// Off unless the operator turns it on.
    pub enabled: bool,
    pub finished: Finished,
    pub terminal: TerminalRing,
}

impl NotifyConfig {
    /// The operator's config: the file under `dir` (absent is the default), with `MARION_NOTIFY`
    /// over its `enabled`. A file marion cannot read or parse is off, and says why.
    pub fn load(dir: Option<&Path>, env: Option<&str>) -> Result<NotifyConfig, String> {
        let mut config = match dir.map(|d| d.join(CONFIG_FILE)) {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(text) => toml::from_str(&text)
                    .map_err(|e| format!("{} is not a notify config: {e}", path.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => NotifyConfig::default(),
                Err(e) => return Err(format!("reading {}: {e}", path.display())),
            },
            None => NotifyConfig::default(),
        };
        match env.map(str::trim) {
            Some("on" | "1" | "true") => config.enabled = true,
            Some("off" | "0" | "false") => config.enabled = false,
            _ => {}
        }
        Ok(config)
    }

    /// Write `self` to `dir`'s `notify.toml`, private to the operator.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        crate::private_fs::create_dir_all(dir)?;
        let text = format!(
            "# marion's desktop notifications (`marion notify on|off`).\nenabled = {}\nfinished = \"{}\"\nterminal = \"{}\"\n",
            self.enabled,
            self.finished.word(),
            self.terminal.word(),
        );
        crate::private_fs::write_atomic(&dir.join(CONFIG_FILE), text.as_bytes())
    }
}

impl Finished {
    /// The word `notify.toml` spells it with.
    pub fn word(self) -> &'static str {
        match self {
            Finished::Roots => "roots",
            Finished::All => "all",
            Finished::Off => "off",
        }
    }
}

impl TerminalRing {
    /// The word `notify.toml` spells it with.
    pub fn word(self) -> &'static str {
        match self {
            TerminalRing::Bell => "bell",
            TerminalRing::Osc9 => "osc9",
            TerminalRing::Osc777 => "osc777",
        }
    }
}

/// Where a notice goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// macOS's `osascript`, the notice's text passed as arguments, never in the script.
    Osascript,
    /// freedesktop's `notify-send`.
    NotifySend,
    /// The terminal of the client that claimed notices (`notify/claim`).
    Terminal,
    /// One JSON line per notice, appended to a file — the tests' fake.
    Record(PathBuf),
    Off,
}

impl Backend {
    /// Resolved once: the override, else the platform's notifier where it exists, else the
    /// terminal.
    pub fn resolve(env: Option<&str>) -> Backend {
        match env.map(str::trim) {
            Some(v) if v.starts_with("record:") => {
                return Backend::Record(PathBuf::from(&v["record:".len()..]));
            }
            Some("terminal") => return Backend::Terminal,
            Some("off") => return Backend::Off,
            Some("osascript") => return Backend::Osascript,
            Some("notify-send") => return Backend::NotifySend,
            _ => {}
        }
        if cfg!(target_os = "macos") && Path::new("/usr/bin/osascript").exists() {
            Backend::Osascript
        } else if on_path("notify-send") {
            Backend::NotifySend
        } else {
            Backend::Terminal
        }
    }

    /// Its name, for `marion notify status`.
    pub fn name(&self) -> String {
        match self {
            Backend::Osascript => "osascript".into(),
            Backend::NotifySend => "notify-send".into(),
            Backend::Terminal => "the terminal of a marion screen that is open".into(),
            Backend::Record(p) => format!("record:{}", p.display()),
            Backend::Off => "off".into(),
        }
    }

    /// The command that shows `shown`, for a process backend. The text rides as arguments.
    fn command(&self, shown: &Shown) -> Option<std::process::Command> {
        let mut c = match self {
            Backend::Osascript => {
                let mut c = std::process::Command::new("/usr/bin/osascript");
                c.args([
                    "-e",
                    "on run argv",
                    "-e",
                    "display notification (item 2 of argv) with title (item 1 of argv)",
                    "-e",
                    "end run",
                    "--",
                ]);
                c
            }
            Backend::NotifySend => {
                let mut c = std::process::Command::new("notify-send");
                c.args(["--app-name=marion", "--"]);
                c
            }
            _ => return None,
        };
        c.arg(&shown.title).arg(&shown.body);
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        Some(c)
    }
}

/// Whether `program` is a file in one of `PATH`'s directories — a `stat`, never a run.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// **What a supervisor builds its notifier from**, kept so `notify/configure` can turn notices on
/// or off while it runs: the config it loaded, the backend it resolved, the project that titles
/// each notice. Only `enabled` changes after the start.
#[derive(Debug, Clone)]
pub struct NotifySeed {
    pub config: NotifyConfig,
    pub backend: Backend,
    pub project: String,
}

impl NotifySeed {
    /// The notifier this seed makes with notices `enabled` or not — `None` where they are off, or
    /// where nothing can show one. A fresh one learns the tree before it tells anything, so turning
    /// notices back on never replays what happened while they were off.
    pub fn notifier(&self, enabled: bool) -> Option<Notifier> {
        Notifier::new(
            NotifyConfig {
                enabled,
                ..self.config
            },
            self.backend.clone(),
            &self.project,
        )
    }
}

/// **The notifier**: what each node's last notice was, and where notices go.
pub struct Notifier {
    config: NotifyConfig,
    backend: Backend,
    title: String,
    state: Mutex<Seen>,
    /// The delivery thread's channel, started at the first process or file notice.
    tx: Mutex<Option<Sender<Shown>>>,
}

#[derive(Default)]
struct Seen {
    /// The registry generation last observed; `None` before the first observation.
    generation: Option<u64>,
    kinds: HashMap<AgentId, Option<NoticeKind>>,
}

impl Notifier {
    /// A notifier for the project named `project` (its directory name titles each notice). `None`
    /// where notifications are off — the supervisor then does no notification work at all.
    pub fn new(config: NotifyConfig, backend: Backend, project: &str) -> Option<Notifier> {
        (config.enabled && backend != Backend::Off).then(|| Notifier {
            config,
            backend,
            title: format!("marion · {project}"),
            state: Mutex::new(Seen::default()),
            tx: Mutex::new(None),
        })
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn ring(&self) -> TerminalRing {
        self.config.terminal
    }

    /// **The notices `tree` warrants since the last observation**: each node whose kind changed to
    /// one worth telling, merged into one where there are more than three. Returns at once when
    /// the generation has not moved; the first call only learns.
    pub fn observe(&self, tree: &Replay, generation: u64) -> Vec<Shown> {
        let mut seen = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let first = seen.generation.is_none();
        if seen.generation == Some(generation) {
            return Vec::new();
        }
        seen.generation = Some(generation);
        let mut notices = Vec::new();
        for node in tree.nodes() {
            let kind = kind_of(node);
            let was = seen.kinds.insert(node.agent_id.clone(), kind);
            if first || was == Some(kind) {
                continue;
            }
            if let Some(k) = kind.filter(|k| self.worth(*k, node)) {
                notices.push(Notice::of(node, k));
            }
        }
        drop(seen);
        self.shown(notices)
    }

    fn worth(&self, kind: NoticeKind, node: &ReplayedNode) -> bool {
        match (kind, self.config.finished) {
            (NoticeKind::Finished, Finished::Off) => false,
            (NoticeKind::Finished, Finished::Roots) => node.parent_id().is_none(),
            _ => true,
        }
    }

    fn shown(&self, notices: Vec<Notice>) -> Vec<Shown> {
        if notices.len() > MERGE_ABOVE {
            let mut counts: Vec<(NoticeKind, usize)> = Vec::new();
            for n in &notices {
                match counts.iter_mut().find(|(k, _)| *k == n.kind) {
                    Some((_, c)) => *c += 1,
                    None => counts.push((n.kind, 1)),
                }
            }
            let parts: Vec<String> = counts
                .iter()
                .map(|(k, c)| format!("{c} {}", k.words()))
                .collect();
            return vec![Shown {
                title: self.title.clone(),
                body: format!("{} nodes: {}", notices.len(), parts.join(", ")),
            }];
        }
        notices
            .into_iter()
            .map(|n| Shown {
                title: self.title.clone(),
                body: n.body(),
            })
            .collect()
    }

    /// Hand `shown` to a process or file backend, on the delivery thread. `false` for the terminal
    /// backend, whose notices the supervisor sends to the claiming client itself.
    pub fn deliver(&self, shown: Vec<Shown>) -> bool {
        if matches!(self.backend, Backend::Terminal | Backend::Off) {
            return false;
        }
        let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if tx.is_none() {
            let (s, r) = std::sync::mpsc::channel::<Shown>();
            let backend = self.backend.clone();
            let spawned = std::thread::Builder::new()
                .name("marion-notify".into())
                .spawn(move || {
                    for shown in r {
                        show(&backend, &shown);
                    }
                });
            match spawned {
                Ok(_) => *tx = Some(s),
                Err(e) => {
                    eprintln!("marion: no thread to deliver notifications: {e}");
                    return true;
                }
            }
        }
        if let Some(tx) = tx.as_ref() {
            for s in shown {
                let _ = tx.send(s);
            }
        }
        true
    }
}

/// Show one notice on a process or file backend. A failure is reported and never retried.
pub fn show(backend: &Backend, shown: &Shown) {
    if let Backend::Record(path) = backend {
        let line = serde_json::to_string(shown).unwrap_or_default() + "\n";
        let written = crate::private_fs::open_append(path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        if let Err(e) = written {
            eprintln!("marion: a notification was not recorded: {e}");
        }
        return;
    }
    let Some(mut command) = backend.command(shown) else {
        return;
    };
    match crate::spawn_receive_gate::SPAWN_RECEIVE_GATE.spawn(&mut command) {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => eprintln!("marion: {} did not start: {e}", backend.name()),
    }
}

/// **`marion notify on | off | status | test`**: turn desktop notifications on or off in
/// `notify.toml`, say what is configured and where notices would go, or show one now.
/// **Turn notifications on or off in the operator's `notify.toml`**, keeping the rest of it — what
/// `marion notify on|off` and Setup's row both do. Answers the file written.
pub fn set_enabled(enabled: bool) -> Result<std::path::PathBuf, String> {
    let dir = crate::credentials::config_dir().map_err(|e| e.to_string())?;
    let config = NotifyConfig::load(Some(&dir), None)?;
    let path = dir.join(CONFIG_FILE);
    NotifyConfig { enabled, ..config }
        .save(&dir)
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}

/// Whether notifications are on — the file, with `MARION_NOTIFY` over it — and how a notice is
/// shown here, as `marion notify status` says both.
pub fn status() -> (bool, String) {
    let enabled = crate::credentials::config_dir()
        .ok()
        .and_then(|dir| {
            NotifyConfig::load(Some(&dir), std::env::var(NOTIFY_ENV).ok().as_deref()).ok()
        })
        .is_some_and(|c| c.enabled);
    let backend = Backend::resolve(std::env::var(BACKEND_ENV).ok().as_deref());
    (enabled, backend.name())
}

pub fn main(args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    let dir = match crate::credentials::config_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("marion: {e}");
            return ExitCode::FAILURE;
        }
    };
    let env = std::env::var(NOTIFY_ENV).ok();
    let config = match NotifyConfig::load(Some(&dir), None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("marion: {e}");
            return ExitCode::FAILURE;
        }
    };
    let backend = Backend::resolve(std::env::var(BACKEND_ENV).ok().as_deref());
    match args.first().map(String::as_str) {
        Some(verb @ ("on" | "off")) => match set_enabled(verb == "on") {
            Ok(path) => {
                println!("marion: notifications {verb}, in {}.", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("marion: {e}");
                ExitCode::FAILURE
            }
        },
        Some("status") | None => {
            let effective = NotifyConfig::load(Some(&dir), env.as_deref())
                .map_or(config.enabled, |c| c.enabled);
            println!(
                "notifications: {}{}",
                if effective { "on" } else { "off" },
                match env.as_deref() {
                    Some(v) => format!(" ({NOTIFY_ENV}={v})"),
                    None => String::new(),
                }
            );
            println!("finished: {}", config.finished.word());
            println!("terminal: {}", config.terminal.word());
            println!("shown by: {}", backend.name());
            println!("config: {}", dir.join(CONFIG_FILE).display());
            ExitCode::SUCCESS
        }
        Some("test") => {
            let shown = Shown {
                title: "marion".into(),
                body: "a test notification: this is how a node's news reaches you".into(),
            };
            match &backend {
                Backend::Terminal => {
                    let mut out = std::io::stdout();
                    let _ = std::io::Write::write_all(
                        &mut out,
                        &terminal_bytes(config.terminal, &shown),
                    );
                    let _ = std::io::Write::flush(&mut out);
                }
                Backend::Off => {
                    eprintln!("marion: {BACKEND_ENV}=off, so nothing was shown");
                    return ExitCode::FAILURE;
                }
                other => show(other, &shown),
            }
            println!(
                "marion: sent a test notification through {}",
                backend.name()
            );
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("marion: `notify {other}` is not a verb; try on, off, status or test");
            ExitCode::from(2)
        }
    }
}

/// The escape a terminal client writes for `shown` under `ring`, between frames.
pub fn terminal_bytes(ring: TerminalRing, shown: &Shown) -> Vec<u8> {
    let clean = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).collect() };
    match ring {
        TerminalRing::Bell => b"\x07".to_vec(),
        TerminalRing::Osc9 => {
            format!("\x1b]9;{}: {}\x07", clean(&shown.title), clean(&shown.body)).into_bytes()
        }
        TerminalRing::Osc777 => format!(
            "\x1b]777;notify;{};{}\x07",
            clean(&shown.title),
            clean(&shown.body)
        )
        .into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::ProcessExit;
    use marion_core::harness::Harness;
    use marion_core::journal::{Exited, RecordKind, SpawnIntent, Spawned, StateChanged};
    use marion_core::node::BlockReason;

    /// A journal the tests append to, read back as the registry would fold it.
    struct Journal {
        _dir: marion_testsupport::Scratch,
        path: PathBuf,
        generation: u64,
    }

    impl Journal {
        fn new(tag: &str) -> Journal {
            let dir = marion_testsupport::scratch(tag);
            let path = dir.join("journal.jsonl");
            Journal {
                _dir: dir,
                path,
                generation: 0,
            }
        }

        fn write(&mut self, kind: RecordKind) {
            crate::journal::append_at(&self.path, kind).unwrap();
            self.generation += 1;
        }

        fn spawn(&mut self, id: &str, parent: Option<&str>) {
            self.write(RecordKind::SpawnIntent(SpawnIntent {
                agent_id: AgentId(id.into()),
                parent_id: parent.map(|p| AgentId(p.into())),
                agent_type: "codex-impl".into(),
                harness: Harness::Codex,
                depth: u32::from(parent.is_some()),
                task_id: None,
                timeout_secs: None,
                verification: vec![],
                review_of: None,
                budget: None,
                race: None,
            }));
            self.write(RecordKind::Spawned(Spawned {
                agent_id: AgentId(id.into()),
                harness_version: "t".into(),
                model: None,
                pid: None,
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }));
        }

        fn state(&mut self, id: &str, state: NodeState) {
            self.write(RecordKind::StateChanged(StateChanged {
                agent_id: AgentId(id.into()),
                state,
                reason: Some("SECRET-REASON-TEXT".into()),
            }));
        }

        fn exit(&mut self, id: &str, status: ExitStatus) {
            self.write(RecordKind::Exited(Exited {
                agent_id: AgentId(id.into()),
                status,
                exit: ProcessExit {
                    code: Some(1),
                    signal: None,
                    description: "SECRET-FAILURE-LINE".into(),
                },
            }));
        }

        fn observe(&self, n: &Notifier) -> Vec<Shown> {
            n.observe(
                &crate::journal::read_path(&self.path).unwrap(),
                self.generation,
            )
        }
    }

    fn on(finished: Finished) -> Notifier {
        Notifier::new(
            NotifyConfig {
                enabled: true,
                finished,
                terminal: TerminalRing::Bell,
            },
            Backend::Record(PathBuf::from("/dev/null")),
            "acme-api",
        )
        .unwrap()
    }

    #[test]
    fn notifications_are_off_unless_the_operator_turns_them_on() {
        assert!(!NotifyConfig::load(None, None).unwrap().enabled);
        assert!(NotifyConfig::load(None, Some("on")).unwrap().enabled);
        assert!(
            Notifier::new(NotifyConfig::default(), Backend::Terminal, "p").is_none(),
            "off means no notifier at all"
        );
        let dir = marion_testsupport::scratch("notify-config");
        let config = NotifyConfig {
            enabled: true,
            finished: Finished::All,
            terminal: TerminalRing::Osc9,
        };
        config.save(&dir).unwrap();
        assert_eq!(NotifyConfig::load(Some(&dir), None).unwrap(), config);
        assert!(!NotifyConfig::load(Some(&dir), Some("off")).unwrap().enabled);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(CONFIG_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "private to the operator");
    }

    /// **Each transition into a kind is told once, and a restart replays nothing**: the first
    /// observation only learns; a block is `needs you`; an unmoved generation is nothing; a child's
    /// clean end is not a notice under `roots`, its root's is.
    #[test]
    fn each_transition_is_told_once_and_the_first_look_only_learns() {
        let mut j = Journal::new("notify-once");
        j.spawn("root-1111", None);
        j.spawn("kid-2222", Some("root-1111"));
        j.state("kid-2222", NodeState::Blocked(BlockReason::Permission));
        let n = on(Finished::Roots);
        assert!(
            j.observe(&n).is_empty(),
            "what was already true is not news"
        );

        j.state("kid-2222", NodeState::Running);
        assert!(j.observe(&n).is_empty());
        j.state("kid-2222", NodeState::Blocked(BlockReason::Permission));
        let told = j.observe(&n);
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].title, "marion · acme-api");
        assert_eq!(
            told[0].body,
            "codex-impl kid-2222 (codex) needs you — blocked:permission"
        );
        assert!(j.observe(&n).is_empty(), "the same generation again");

        j.exit("kid-2222", ExitStatus::Ok);
        assert!(j.observe(&n).is_empty(), "a child's clean end, under roots");
        j.exit("root-1111", ExitStatus::Ok);
        assert_eq!(
            j.observe(&n)[0].body,
            "codex-impl root-1111 (codex) finished — exited:ok"
        );
    }

    /// **A notice says only its enumerated fields**: the state's reason and the exit's description
    /// — words marion or a harness wrote — never reach it.
    #[test]
    fn a_notice_carries_no_words_but_its_enumerated_fields() {
        let mut j = Journal::new("notify-scrub");
        j.spawn("root-1111", None);
        let n = on(Finished::All);
        j.observe(&n);
        j.state("root-1111", NodeState::Blocked(BlockReason::Permission));
        j.exit("root-1111", ExitStatus::Failed);
        let told = j.observe(&n);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told.iter()
                .all(|s| !s.body.contains("SECRET") && !s.title.contains("SECRET")),
            "{told:?}"
        );
        assert!(told[0].body.contains("failed"), "{told:?}");
    }

    #[test]
    fn more_than_three_in_one_look_are_merged_into_one() {
        let mut j = Journal::new("notify-merge");
        for id in ["a-1", "b-2", "c-3", "d-4", "e-5"] {
            j.spawn(id, None);
        }
        let n = on(Finished::All);
        j.observe(&n);
        for id in ["a-1", "b-2", "c-3"] {
            j.exit(id, ExitStatus::Failed);
        }
        j.exit("d-4", ExitStatus::Ok);
        j.exit("e-5", ExitStatus::TimedOut);
        let told = j.observe(&n);
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].body, "5 nodes: 3 failed, 1 finished, 1 hit a limit");
    }

    /// Every state has an answer, and a cancel is nobody's news unless a budget made it.
    #[test]
    fn every_state_has_one_kind_and_a_plain_cancel_has_none() {
        let mut j = Journal::new("notify-kinds");
        j.spawn("n-1", None);
        let mut node = crate::journal::read_path(&j.path).unwrap().nodes()[0].clone();
        let expect = [
            (NodeState::Running, ReapState::Live, None),
            (NodeState::Idle, ReapState::Live, None),
            (
                NodeState::Blocked(BlockReason::Permission),
                ReapState::Live,
                Some(NoticeKind::NeedsYou),
            ),
            (
                NodeState::Running,
                ReapState::Orphaned,
                Some(NoticeKind::Failed),
            ),
            (NodeState::Idle, ReapState::ReapedIdle, None),
            (
                NodeState::Exited(ExitStatus::Ok),
                ReapState::Live,
                Some(NoticeKind::Finished),
            ),
            (
                NodeState::Exited(ExitStatus::Unreported),
                ReapState::Live,
                Some(NoticeKind::Failed),
            ),
            (
                NodeState::Exited(ExitStatus::Killed),
                ReapState::Live,
                Some(NoticeKind::Failed),
            ),
            (
                NodeState::Exited(ExitStatus::TimedOut),
                ReapState::Live,
                Some(NoticeKind::Limit),
            ),
            (
                NodeState::Exited(ExitStatus::Cancelled),
                ReapState::Live,
                None,
            ),
        ];
        for (state, reap, want) in expect {
            node.state = state;
            node.reap_state = reap;
            assert_eq!(kind_of(&node), want, "{state:?} {reap:?}");
        }
        node.state = NodeState::Exited(ExitStatus::Cancelled);
        node.cancel = Some(marion_core::registry::CancelView {
            by: CancelBy::Budget {
                owner: AgentId("r".into()),
                scope: marion_core::budget::BudgetScope::Tree,
                spent: 2,
                limit: 1,
            },
            forced: false,
            verb: "none".into(),
        });
        assert_eq!(kind_of(&node), Some(NoticeKind::Limit), "a spent budget");
    }

    #[test]
    fn the_record_backend_appends_one_line_per_notice_and_others_resolve_by_name() {
        let dir = marion_testsupport::scratch("notify-record");
        let path = dir.join("notices.jsonl");
        let backend = Backend::resolve(Some(&format!("record:{}", path.display())));
        assert_eq!(backend, Backend::Record(path.clone()));
        let shown = Shown {
            title: "t".into(),
            body: "b".into(),
        };
        show(&backend, &shown);
        show(&backend, &shown);
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().count(), 2);
        assert_eq!(
            serde_json::from_str::<Shown>(lines.lines().next().unwrap()).unwrap(),
            shown
        );
        assert_eq!(Backend::resolve(Some("terminal")), Backend::Terminal);
        assert_eq!(Backend::resolve(Some("off")), Backend::Off);
    }

    /// The osascript notice passes its text as arguments to a fixed script, so no text can become
    /// AppleScript.
    #[test]
    fn a_process_backend_carries_the_text_as_arguments_never_in_the_script() {
        let shown = Shown {
            title: "marion · p".into(),
            body: "\" & do shell script \"x".into(),
        };
        let c = Backend::Osascript.command(&shown).unwrap();
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[args.len() - 2..],
            [shown.title.clone(), shown.body.clone()]
        );
        assert!(
            args[..args.len() - 2]
                .iter()
                .all(|a| !a.contains("do shell"))
        );
    }

    #[test]
    fn a_terminal_escape_carries_no_control_characters_from_its_text() {
        let shown = Shown {
            title: "t".into(),
            body: "a\x1b]0;evil\x07b".into(),
        };
        assert_eq!(terminal_bytes(TerminalRing::Bell, &shown), b"\x07");
        let osc = terminal_bytes(TerminalRing::Osc9, &shown);
        assert_eq!(osc, b"\x1b]9;t: a]0;evilb\x07");
    }
}
