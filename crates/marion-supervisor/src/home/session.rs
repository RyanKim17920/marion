//! The home screen's loop: paint, keep the forest and the selected node current, read the
//! keyboard, carry out what the keys asked for.
//!
//! # One thread for the screen, as the tree screen had
//!
//! The keyboard is glanced at on this thread and nowhere else: an attach started from here reads
//! the same stdin, and a reader thread still blocked in `read(2)` would steal the operator's first
//! keystroke into the pane. The slow things run beside it and report over a channel — the doctor
//! probe (a `--version` per harness, seconds in all) and a `marion run` (which may start a
//! supervisor) — so neither holds up a frame.
//!
//! # Leaving and coming back
//!
//! An attach, a resume, a shell, a diff and an editor each need the whole terminal, so the loop
//! leaves the screen, runs the command in the foreground and re-enters — the pattern `marion tree`
//! used for its attach. The state survives the trip; only the terminal changes hands.

use super::view::{self, Places};
use super::{AgentType, Effect, Harness, Home};
use crate::doctor::{self, SurfaceRole};
use crate::tree::Subscription;
use marion_core::contract::AgentId;
use marion_core::proto::params::ActivityCursor;
use marion_tui::home::keys::{self, Key};
use marion_tui::home::{Ready, Screen as HomeScreen, Tab, Theme};
use marion_tui::{Screen, ScreenBackend, Sticky};
use ratatui::Terminal;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// How long the keyboard is waited on when there is no supervisor stream to pace the loop.
const POLL: Duration = Duration::from_millis(50);
/// How often a missing supervisor is looked for again.
const REDIAL: Duration = Duration::from_secs(1);
/// How often the selected node's detail and stream are asked for.
const DETAIL: Duration = Duration::from_millis(400);

/// Where the home screen is looking.
#[derive(Debug, Clone)]
pub struct Options {
    pub repo: PathBuf,
    pub state: PathBuf,
    /// The tab it opens on: Start for bare `marion`, Watch for `marion ls`.
    pub tab: Tab,
}

/// What needs the whole terminal, so the loop leaves the screen for it.
enum Handoff {
    Quit,
    Attach(String),
    Resume(AgentId),
    Shell(PathBuf),
    Diff(String),
    EditTypes,
}

/// What the side threads report.
enum Report {
    /// Doctor's rows for one harness.
    Doctor(Vec<doctor::Row>),
    DoctorDone,
    /// A `marion run` finished: whether it started, its last line, and the root it named.
    Ran {
        ok: bool,
        line: String,
        root: Option<String>,
    },
}

/// Run the home screen until the operator quits.
pub fn run(opts: &Options) -> Result<(), String> {
    let mut s = Session::new(opts);
    loop {
        let (cols, rows) = marion_tui::guard::window_size(0).unwrap_or((80, 24));
        let screen = Screen::enter(std::io::stdout(), 0, &Sticky::initial(cols, rows))
            .map_err(|e| format!("entering the terminal: {e}"))?;
        match s.draw_loop(screen, cols, rows)? {
            Handoff::Quit => return Ok(()),
            Handoff::Attach(id) => {
                if let Err(e) = crate::attach::run(&id, &s.repo, &s.state) {
                    s.home.notice = Some(format!("attach refused: {e}"));
                }
            }
            Handoff::Resume(id) => {
                let c = s.marion(&["resume", &id.0]);
                s.foreground(c, "resume");
            }
            Handoff::Shell(dir) => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
                let mut c = std::process::Command::new(shell);
                c.current_dir(dir);
                s.foreground(c, "shell");
            }
            Handoff::Diff(branch) => {
                let mut c = std::process::Command::new("git");
                c.arg("-C")
                    .arg(&s.repo)
                    .args(["log", "-p", "--stat", &format!("HEAD..{branch}")]);
                s.foreground(c, "git log");
            }
            Handoff::EditTypes => {
                let file = s.repo.join(crate::run::AGENT_TYPES_FILE);
                if let Some(dir) = file.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
                let mut c = std::process::Command::new(editor);
                c.arg(&file);
                s.foreground(c, "editor");
                s.validate_types();
            }
        }
    }
}

struct Session {
    home: Home,
    places: Places,
    repo: PathBuf,
    state: PathBuf,
    socket: PathBuf,
    theme: Theme,
    sub: Option<Subscription>,
    last_dial: Option<Instant>,
    last_detail: Option<Instant>,
    /// A root a run just started, to select once the forest shows it.
    pending_select: Option<String>,
    reports: Receiver<Report>,
    tx: Sender<Report>,
    tick: usize,
}

impl Session {
    fn new(opts: &Options) -> Session {
        let key = crate::socket::project_root(&opts.repo);
        let socket = crate::socket::socket_paths(&opts.state, &key, crate::socket::own_uid())
            .socket()
            .to_path_buf();
        let shown = match key.file_name().and_then(|f| f.to_str()) {
            Some(".git") => key.parent().unwrap_or(&key).to_path_buf(),
            _ => key.clone(),
        };
        crate::recent::touch_project(
            &marion_core::paths::ProjectDir::new(&opts.state, &key),
            &key,
        );
        let (tx, reports) = channel();
        let mut s = Session {
            home: Home::new(opts.tab),
            places: Places {
                project: view::project_label(
                    &shown.to_string_lossy(),
                    std::env::var("HOME").ok().as_deref(),
                    current_branch(&shown).as_deref(),
                ),
                state: view::tilde(&opts.state.to_string_lossy()),
            },
            repo: opts.repo.clone(),
            state: opts.state.clone(),
            socket,
            theme: Theme::from_env(),
            sub: None,
            last_dial: None,
            last_detail: None,
            pending_select: None,
            reports,
            tx,
            tick: 0,
        };
        s.load_types();
        s.recheck();
        s
    }

    /// One stay on the screen: until the operator quits or asks for something that needs the
    /// whole terminal. The terminal is restored on every way out.
    fn draw_loop(&mut self, screen: Screen, cols: u16, rows: u16) -> Result<Handoff, String> {
        let mut stdin = marion_tui::guard::Keyboard::open(0)
            .map_err(|e| format!("watching the keyboard: {e}"))?;
        let mut terminal = Terminal::new(ScreenBackend::new(screen, cols, rows))
            .map_err(|e| format!("starting the renderer: {e}"))?;
        let leave = |t: &Terminal<ScreenBackend>| t.backend().screen().leave();
        let mut size = (cols, rows);
        let mut buf = [0u8; 1024];
        loop {
            if let Some(now) = marion_tui::guard::window_size(0)
                && now != size
            {
                size = now;
                terminal.backend_mut().set_size(size.0, size.1);
            }
            self.reports();
            self.follow_forest();
            self.follow_selection();
            self.paint(&mut terminal);
            self.tick = self.tick.wrapping_add(1);
            // With a subscription its read bound paced this pass; without one the keyboard does.
            let wait = if self.sub.is_some() {
                Duration::ZERO
            } else {
                POLL
            };
            let n = match stdin.read_within(&mut buf, wait) {
                Ok(Some(0)) => {
                    leave(&terminal);
                    return Ok(Handoff::Quit);
                }
                Ok(Some(n)) => n,
                Ok(None) | Err(_) => 0,
            };
            for key in keys::decode(&buf[..n]) {
                if let Some(handoff) = self.key(key) {
                    leave(&terminal);
                    return Ok(handoff);
                }
            }
        }
    }

    /// One key through the state machine, and its effect carried out. `Some` when it needs the
    /// whole terminal.
    fn key(&mut self, key: Key) -> Option<Handoff> {
        self.home.notice = None;
        match self.home.key(key) {
            Effect::None => None,
            Effect::Quit => Some(Handoff::Quit),
            Effect::Attach(id) => match self.home.selected().map(crate::tree::open_target) {
                Some(Ok(_)) => Some(Handoff::Attach(id.0)),
                Some(Err(why)) => {
                    self.home.notice = Some(why);
                    None
                }
                None => None,
            },
            Effect::Resume(id) => Some(Handoff::Resume(id)),
            Effect::Shell(dir) => Some(Handoff::Shell(dir)),
            Effect::Diff(branch) => Some(Handoff::Diff(branch)),
            Effect::EditTypes => Some(Handoff::EditTypes),
            Effect::Steer(id, text) => {
                self.home.notice = Some(
                    match crate::courier::steer(&self.socket, &id, &text, None) {
                        Ok(s) => s.sentence(),
                        Err(e) => format!("steer refused: {e}"),
                    },
                );
                None
            }
            Effect::Cancel(id) => {
                self.home.notice = Some(match crate::courier::kill(&self.socket, &id) {
                    Ok(_) => format!("{} cancelled", crate::tree::short_id(&id.0)),
                    Err(e) => format!("cancel refused: {e}"),
                });
                None
            }
            Effect::Copy(text) => {
                copy(&text);
                self.home.notice = Some(format!("copied: {text}"));
                None
            }
            Effect::Recheck => {
                self.recheck();
                None
            }
            run @ Effect::Run { .. } => {
                self.start_run(&run);
                None
            }
        }
    }

    /// `marion run … --detach` on a thread: this binary, the echoed argv, the project stated.
    fn start_run(&mut self, run: &Effect) {
        let Some(argv) = run.argv() else { return };
        if let Effect::Run {
            agent_type,
            model: Some(model),
            ..
        } = run
            && let Some(t) = self.home.types.iter().find(|t| t.name == *agent_type)
        {
            crate::recent::remember_model(&self.state, &t.harness, model);
            self.load_models();
        }
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        let mut c = self.marion(&args);
        let tx = self.tx.clone();
        self.home.notice = Some("starting…".into());
        std::thread::spawn(move || {
            let report = match c.stdin(std::process::Stdio::null()).output() {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let said = stdout
                        .lines()
                        .chain(stderr.lines())
                        .rfind(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .to_string();
                    let root = started_root(&stdout).or_else(|| started_root(&stderr));
                    Report::Ran {
                        ok: out.status.success(),
                        line: match &root {
                            Some(id) => format!("started {}", crate::tree::short_id(id)),
                            None => said,
                        },
                        root,
                    }
                }
                Err(e) => Report::Ran {
                    ok: false,
                    line: format!("marion run did not start: {e}"),
                    root: None,
                },
            };
            let _ = tx.send(report);
        });
    }

    /// Keep the forest current: dial when there is no subscription (at most once a [`REDIAL`]),
    /// fold one notification when there is.
    fn follow_forest(&mut self) {
        match &mut self.sub {
            Some(sub) => match sub.poll() {
                Ok(Some(_)) => {
                    let nodes = sub.nodes.clone();
                    self.home.set_nodes(nodes);
                }
                Ok(None) => {}
                Err(_) => {
                    self.sub = None;
                    self.home.lost_supervisor();
                }
            },
            None => {
                if self.last_dial.is_some_and(|t| t.elapsed() < REDIAL) {
                    return;
                }
                self.last_dial = Some(Instant::now());
                if let Ok(sub) = Subscription::open(&self.repo, &self.state) {
                    self.home.set_nodes(sub.nodes.clone());
                    self.sub = Some(sub);
                }
            }
        }
        if let Some(id) = self.pending_select.clone()
            && self.home.select(&id)
        {
            self.pending_select = None;
        }
    }

    /// Ask for the selected node's detail and the next page of its stream, at most every
    /// [`DETAIL`], while Watch is showing.
    fn follow_selection(&mut self) {
        if self.home.tab != Tab::Watch || self.sub.is_none() {
            return;
        }
        if self.last_detail.is_some_and(|t| t.elapsed() < DETAIL) {
            return;
        }
        self.last_detail = Some(Instant::now());
        let Some(id) = self.home.selected().map(|n| n.agent_id.clone()) else {
            return;
        };
        let cursor = match (&self.home.watch.detail, self.home.watch.next) {
            (Some((d, _)), Some(next)) if *d == id => ActivityCursor::From(next),
            _ => ActivityCursor::Tail,
        };
        if let Ok(r) = crate::courier::node_get_with(&self.socket, &id, Some(cursor)) {
            self.home.absorb_detail(id, r.detail);
        }
    }

    fn paint(&mut self, terminal: &mut Terminal<ScreenBackend>) {
        let frame = view::frame(&self.home, &self.places);
        let screen = HomeScreen {
            theme: self.theme,
            project: &self.places.project,
            attention: frame.attention,
            body: frame.body(self.home.tab),
            input: frame.input.clone(),
            hints: frame.hints.clone(),
            notice: self.home.notice.clone(),
            frame: self.tick / 2,
        };
        let _ = terminal.draw(|f| f.render_widget(&screen, f.area()));
    }

    /// `marion <args>` for this project: this same binary, with the project it was opened on
    /// stated so the child cannot resolve a different one.
    fn marion(&self, args: &[&str]) -> std::process::Command {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("marion"));
        let mut c = std::process::Command::new(exe);
        c.args(args)
            .arg("--repo")
            .arg(&self.repo)
            .arg("--state-dir")
            .arg(&self.state);
        c
    }

    /// Run `c` with the terminal, and say how it ended if it failed.
    fn foreground(&mut self, mut c: std::process::Command, what: &str) {
        match c.status() {
            Ok(st) if st.success() => {}
            Ok(st) => self.home.notice = Some(format!("{what} exited {st}")),
            Err(e) => self.home.notice = Some(format!("{what} did not start: {e}")),
        }
    }

    fn load_types(&mut self) {
        match crate::run::agent_types(&self.repo) {
            Ok(types) => {
                let user: Vec<&str> = types.user().iter().map(|t| t.name.as_str()).collect();
                self.home.types = types
                    .names()
                    .iter()
                    .filter_map(|n| types.resolve(n))
                    .map(|t| AgentType {
                        harness: harness_name(t.harness, t.acp_agent.as_deref()),
                        writes: t.writes_files(),
                        custom: user.contains(&t.name.as_str()),
                        name: t.name.clone(),
                    })
                    .collect();
            }
            Err(e) => self.home.notice = Some(e.to_string()),
        }
        self.load_models();
    }

    fn load_models(&mut self) {
        let mut names: Vec<String> = self.home.types.iter().map(|t| t.harness.clone()).collect();
        names.sort();
        names.dedup();
        self.home.recent_models = names
            .into_iter()
            .map(|n| {
                let models = crate::recent::models(&self.state, &n);
                (n, models)
            })
            .collect();
    }

    fn validate_types(&mut self) {
        self.home.setup.validation = Some(match crate::run::agent_types(&self.repo) {
            Ok(t) => (
                true,
                format!(
                    "{}: {} custom types, valid",
                    crate::run::AGENT_TYPES_FILE,
                    t.user().len()
                ),
            ),
            Err(e) => (false, e.to_string()),
        });
        self.load_types();
    }

    /// Probe every harness again, on a thread, one harness at a time so rows arrive as they are
    /// read rather than all at the end.
    fn recheck(&mut self) {
        use marion_core::harness::Harness as H;
        self.home.checking = true;
        self.home.harnesses = H::ALL
            .iter()
            .filter(|h| **h != H::Acp)
            .map(|h| Harness {
                name: h.cli_name().to_string(),
                version: None,
                ready: Ready::Checking,
                note: "checking…".into(),
                surfaces: String::new(),
                fix: None,
                detail: Vec::new(),
            })
            .collect();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for h in H::ALL {
                let rows = doctor::run(&doctor::Options {
                    mode: marion_core::proto::model::ProbeMode::Capabilities,
                    harness: Some(h),
                    model: None,
                    acp_command: None,
                });
                if tx.send(Report::Doctor(rows)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Report::DoctorDone);
        });
    }

    fn reports(&mut self) {
        while let Ok(r) = self.reports.try_recv() {
            match r {
                Report::Doctor(rows) => {
                    for h in harnesses_of(&rows) {
                        match self.home.harnesses.iter_mut().find(|x| x.name == h.name) {
                            Some(slot) => *slot = h,
                            None => self.home.harnesses.push(h),
                        }
                    }
                }
                Report::DoctorDone => self.home.checking = false,
                Report::Ran { ok, line, root } => {
                    self.home.notice = Some(line);
                    if ok {
                        self.home.tab = Tab::Watch;
                        // Look again now: the run may have just started this project's supervisor.
                        self.last_dial = None;
                        self.pending_select = root;
                    }
                }
            }
        }
    }
}

/// The branch checked out at `dir`, read once when the screen opens; `None` outside git.
fn current_branch(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The root a `marion run` line names: `marion: root <id> (…) started…`.
fn started_root(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let rest = l.strip_prefix("marion: root ")?;
        rest.split_whitespace().next().map(str::to_string)
    })
}

/// Put `text` on the clipboard with OSC 52, which most terminals honour and none execute.
fn copy(text: &str) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
}

/// The name a harness is shown and matched under — the one doctor's rows carry (`doctor::label`):
/// its command name, or `acp <agent>` for an ACP row.
fn harness_name(h: marion_core::harness::Harness, acp_agent: Option<&str>) -> String {
    match (h, acp_agent) {
        (marion_core::harness::Harness::Acp, Some(a)) => format!("acp {a}"),
        _ => h.cli_name().to_string(),
    }
}

/// Doctor's rows for one probe, as the home screen's entries: one per label, its readiness from
/// the node row by doctor's own rule, its surfaces from every row.
fn harnesses_of(rows: &[doctor::Row]) -> Vec<Harness> {
    rows.iter()
        .filter(|r| r.role == SurfaceRole::Node)
        .map(|r| {
            let name = doctor::label(r);
            let pane = rows
                .iter()
                .any(|p| p.role == SurfaceRole::Pane && doctor::label(p) == name);
            let (ready, note, fix) = match doctor::why_not_ready(r) {
                None => (Ready::Ready, "ready".to_string(), None),
                Some(_) if doctor::not_installed(r) => (
                    Ready::Absent,
                    "not installed".to_string(),
                    Some("install it on $PATH, then `r` to re-check".to_string()),
                ),
                Some(why) => (
                    Ready::Broken,
                    why,
                    Some("`marion doctor` says why; fix it, then `r` to re-check".to_string()),
                ),
            };
            let detail = r
                .report
                .notes
                .iter()
                .filter_map(|n| {
                    let (k, v) = n.split_once(": ")?;
                    matches!(k, "binary" | "version" | "approval")
                        .then(|| (k.to_string(), v.to_string()))
                })
                .collect();
            Harness {
                name,
                version: r.report.harness_version.clone(),
                ready,
                note,
                surfaces: if pane { "headless · pane" } else { "headless" }.into(),
                fix,
                detail,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_started_root_is_read_off_runs_own_line() {
        assert_eq!(
            started_root("marion: root 01a0-3c33 (codex) started, detached; …").as_deref(),
            Some("01a0-3c33")
        );
        assert_eq!(
            started_root("noise\nmarion: root abc (claude, pane) started").as_deref(),
            Some("abc")
        );
        assert_eq!(started_root("marion: nothing"), None);
    }
}
