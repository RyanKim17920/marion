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

use super::wake::{self, Wake, Waker};

/// A side thread's way to report: the report on the channel, then a wake so the loop reads it.
#[derive(Clone)]
struct Reporter {
    tx: Sender<Report>,
    waker: Waker,
}

impl Reporter {
    /// `false` once the screen has gone and nobody will read it.
    fn send(&self, r: Report) -> bool {
        let sent = self.tx.send(r).is_ok();
        self.waker.wake();
        sent
    }
}

/// How often a missing supervisor is looked for again: the one wait with nothing to wake it,
/// since no descriptor exists until a supervisor does.
const REDIAL: Duration = Duration::from_secs(1);
/// How often the screen redraws while something on it moves with time — a running node's
/// elapsed time and spinner, a harness still being checked — and how often a running selected
/// node's detail and stream are read again. Nothing ticks while nothing moves.
const TICK: Duration = Duration::from_secs(1);

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
    /// `marion login <credential id>` / `marion logout <credential id>`, in the foreground.
    Login(String),
    Logout(String),
}

/// What the side threads report.
enum Report {
    /// Doctor's rows for one harness.
    Doctor(Vec<doctor::Row>),
    DoctorDone,
    /// The stored keys, as `marion login --list` finds them, and the store's name; or why not.
    Logins(Result<(Vec<crate::login::ProviderLogins>, String), String>),
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
            Handoff::Login(id) => s.credential("login", &id),
            Handoff::Logout(id) => s.credential("logout", &id),
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
    /// The selected node's detail must be read again: the selection, the tab or the forest
    /// changed, or a tick passed over a running node.
    detail_stale: bool,
    /// A root a run just started, to select once the forest shows it.
    pending_select: Option<String>,
    /// The agents file as the form's preview read it: a write goes ahead only onto the same text.
    preview_base: Option<String>,
    reports: Receiver<Report>,
    reporter: Reporter,
    wake: Wake,
    /// Seconds of animation: spinners advance one frame per tick.
    tick: usize,
    next_tick: Instant,
    /// Something changed since the last paint.
    dirty: bool,
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
        let wake = Wake::new().expect("a socket pair for the screen's wakes");
        let reporter = Reporter {
            tx,
            waker: wake.waker(),
        };
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
            detail_stale: true,
            pending_select: None,
            preview_base: None,
            reports,
            reporter,
            wake,
            tick: 0,
            next_tick: Instant::now(),
            dirty: true,
        };
        s.load_types();
        s.recheck();
        s.load_logins();
        s
    }

    /// One stay on the screen: until the operator quits or asks for something that needs the
    /// whole terminal. The terminal is restored on every way out.
    ///
    /// Event-driven: it paints only when something changed, then sleeps in one `poll(2)` over the
    /// keyboard, the supervisor's stream and the wake pair until one of them has something. The
    /// timeout is the soonest thing the screen needs on its own — a [`TICK`] while something on it
    /// moves, a [`REDIAL`] while no supervisor is there — and none at all otherwise, so an idle
    /// screen over an idle forest makes no wakeups.
    fn draw_loop(&mut self, screen: Screen, cols: u16, rows: u16) -> Result<Handoff, String> {
        let mut stdin = marion_tui::guard::Keyboard::open(0)
            .map_err(|e| format!("watching the keyboard: {e}"))?;
        let mut terminal = Terminal::new(ScreenBackend::new(screen, cols, rows))
            .map_err(|e| format!("starting the renderer: {e}"))?;
        let leave = |t: &Terminal<ScreenBackend>| t.backend().screen().leave();
        self.wake.watch_resizes();
        let mut size = (cols, rows);
        let mut buf = [0u8; 1024];
        self.dirty = true;
        loop {
            self.timers();
            self.follow_selection();
            if self.dirty {
                self.paint(&mut terminal);
                self.dirty = false;
            }
            let timeout = self.timeout();
            // SAFETY: fd 0 is the operator's terminal, open for the whole of this loop.
            let stdin_fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(0) };
            let mut fds = vec![stdin_fd, self.wake.fd()];
            if let Some(sub) = &self.sub {
                fds.push(sub.fd());
            }
            let ready = wake::wait(&fds, timeout).unwrap_or_else(|_| vec![false; fds.len()]);
            drop(fds);
            if ready[1] {
                if self.wake.drain()
                    && let Some(now) = marion_tui::guard::window_size(0)
                    && now != size
                {
                    size = now;
                    terminal.backend_mut().set_size(size.0, size.1);
                    self.dirty = true;
                }
                self.reports();
            }
            if ready.get(2).copied().unwrap_or(false) {
                self.follow_forest();
            }
            if ready[0] {
                let n = match stdin.read_within(&mut buf, Duration::ZERO) {
                    Ok(Some(0)) => {
                        leave(&terminal);
                        return Ok(Handoff::Quit);
                    }
                    Ok(Some(n)) => n,
                    Ok(None) | Err(_) => 0,
                };
                for key in keys::decode(&buf[..n]) {
                    self.dirty = true;
                    if let Some(handoff) = self.key(key) {
                        leave(&terminal);
                        return Ok(handoff);
                    }
                    // A key may have moved the selection or come to Watch: read what it shows.
                    self.detail_stale |= self.home.tab == Tab::Watch;
                }
            }
        }
    }

    /// How long the loop may sleep: until the next tick while something on screen moves, until
    /// the next look for a supervisor while there is none, else for as long as nothing happens.
    fn timeout(&self) -> Option<Duration> {
        let now = Instant::now();
        let tick = self
            .home
            .animating()
            .then(|| self.next_tick.saturating_duration_since(now));
        let redial = self.sub.is_none().then(|| match self.last_dial {
            Some(t) => REDIAL.saturating_sub(t.elapsed()),
            None => Duration::ZERO,
        });
        tick.into_iter().chain(redial).min()
    }

    /// What is due on the clock: a tick, and a look for a missing supervisor.
    fn timers(&mut self) {
        let now = Instant::now();
        if now >= self.next_tick {
            self.next_tick = now + TICK;
            if self.home.animating() {
                self.tick = self.tick.wrapping_add(1);
                self.dirty = true;
                // A running selected node's stream moves with it.
                self.detail_stale |= self.home.tab == Tab::Watch;
            }
        }
        if self.sub.is_none() && self.last_dial.is_none_or(|t| t.elapsed() >= REDIAL) {
            self.dial();
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
            Effect::Login(id) => Some(Handoff::Login(id)),
            Effect::Logout(id) => Some(Handoff::Logout(id)),
            Effect::PreviewType(draft) => {
                self.preview_type(&draft);
                None
            }
            Effect::WriteTypes { text, draft } => {
                self.write_types(&text, &draft.name);
                None
            }
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
        let tx = self.reporter.clone();
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
            tx.send(report);
        });
    }

    /// Look for this project's supervisor, and subscribe to its forest when there is one.
    fn dial(&mut self) {
        self.last_dial = Some(Instant::now());
        if let Ok(mut sub) = Subscription::open(&self.repo, &self.state) {
            if sub.nonblocking().is_err() {
                return;
            }
            self.home.set_nodes(sub.nodes.clone());
            self.sub = Some(sub);
            self.forest_moved();
        }
    }

    /// Fold what the supervisor's stream has sent; a closed stream is a supervisor gone.
    fn follow_forest(&mut self) {
        let Some(sub) = &mut self.sub else { return };
        match sub.drain() {
            Ok(true) => {
                let nodes = sub.nodes.clone();
                self.home.set_nodes(nodes);
                self.forest_moved();
            }
            Ok(false) => {}
            Err(_) => {
                self.sub = None;
                self.home.lost_supervisor();
                self.dirty = true;
            }
        }
    }

    fn forest_moved(&mut self) {
        self.dirty = true;
        self.detail_stale = true;
        if let Some(id) = self.pending_select.clone()
            && self.home.select(&id)
        {
            self.pending_select = None;
        }
    }

    /// Read the selected node's detail and the next page of its stream, while Watch shows it:
    /// when it is stale (a new selection, a forest change, a tick over a running node), never on a
    /// timer of its own.
    fn follow_selection(&mut self) {
        if !self.detail_stale || self.home.tab != Tab::Watch || self.sub.is_none() {
            return;
        }
        self.detail_stale = false;
        let Some(id) = self.home.selected().map(|n| n.agent_id.clone()) else {
            return;
        };
        let cursor = match (&self.home.watch.detail, self.home.watch.next) {
            (Some((d, _)), Some(next)) if *d == id => ActivityCursor::From(next),
            _ => ActivityCursor::Tail,
        };
        if let Ok(r) = crate::courier::node_get_with(&self.socket, &id, Some(cursor)) {
            self.home.absorb_detail(id, r.detail);
            self.dirty = true;
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
            frame: self.tick,
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

    /// `marion login|logout <id>` with the terminal, then the keys listed again. Not
    /// [`Session::marion`]: login takes no project flags, and its key prompt reads the terminal
    /// itself with echo off, so the key never passes through this process.
    fn credential(&mut self, verb: &str, id: &str) {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("marion"));
        let mut c = std::process::Command::new(exe);
        c.args([verb, id]);
        self.foreground(c, &format!("marion {verb}"));
        if self.home.notice.is_none() {
            self.home.notice = Some(format!("marion {verb} {id}: done"));
        }
        self.load_logins();
    }

    /// List the stored keys on a thread: the Keychain answers one `security` call per id, which is
    /// too slow for a frame.
    fn load_logins(&mut self) {
        let tx = self.reporter.clone();
        std::thread::spawn(move || {
            tx.send(Report::Logins(crate::login::user_logins()));
        });
    }

    /// The form's draft applied to the agents file as it is now, held to the spawn path's loader,
    /// and shown as a diff waiting for `y` — or the loader's refusal, back on the form.
    fn preview_type(&mut self, draft: &super::types_form::Draft) {
        let path = self.repo.join(crate::run::AGENT_TYPES_FILE);
        let old = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return self.home.form_refused(format!("reading it: {e}")),
        };
        let new = match super::types_form::apply(&old, draft) {
            Ok(t) => t,
            Err(e) => return self.home.form_refused(e),
        };
        if let Err(e) = crate::run::agent_types_text(path, &new) {
            return self.home.form_refused(e.to_string());
        }
        let diff = super::types_form::diff(&old, &new);
        if diff.is_empty() {
            return self
                .home
                .form_refused("the file already says exactly this".into());
        }
        self.preview_base = Some(old);
        self.home.show_preview(new, diff);
    }

    /// Write the previewed file — only if it is still the file the preview was made from, and only
    /// if the loader still takes the result — then validate and reload the types as `e` does.
    fn write_types(&mut self, text: &str, name: &str) {
        let path = self.repo.join(crate::run::AGENT_TYPES_FILE);
        let base = self.preview_base.take();
        let now = match std::fs::read_to_string(&path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
            Err(_) => None,
        };
        if base.is_none() || now != base {
            self.home.notice = Some(format!(
                "{} changed since the preview; nothing written — press n and preview again",
                crate::run::AGENT_TYPES_FILE
            ));
            return;
        }
        if let Err(e) = crate::run::agent_types_text(path.clone(), text) {
            self.home.notice = Some(format!("not written: {e}"));
            return;
        }
        match super::types_form::write(&path, text) {
            Ok(()) => {
                self.validate_types();
                self.home.notice =
                    Some(format!("wrote {name} to {}", crate::run::AGENT_TYPES_FILE));
            }
            Err(e) => self.home.notice = Some(format!("not written: {e}")),
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
        let tx = self.reporter.clone();
        std::thread::spawn(move || {
            for h in H::ALL {
                let rows = doctor::run(&doctor::Options {
                    mode: marion_core::proto::model::ProbeMode::Capabilities,
                    harness: Some(h),
                    model: None,
                    acp_command: None,
                });
                if !tx.send(Report::Doctor(rows)) {
                    return;
                }
            }
            tx.send(Report::DoctorDone);
        });
    }

    fn reports(&mut self) {
        while let Ok(r) = self.reports.try_recv() {
            self.dirty = true;
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
                Report::Logins(Ok((providers, store))) => {
                    let s = &mut self.home.setup;
                    s.providers = providers
                        .iter()
                        .filter(|p| p.needs_key)
                        .map(|p| p.id.clone())
                        .collect();
                    s.logins = providers
                        .iter()
                        .flat_map(|p| {
                            p.stored.iter().map(|k| super::StoredLogin {
                                provider: p.id.clone(),
                                id: k.id.clone(),
                                note: k.unreadable.clone(),
                            })
                        })
                        .collect();
                    s.store = Some(store);
                    s.logins_error = None;
                }
                Report::Logins(Err(e)) => self.home.setup.logins_error = Some(e),
                Report::Ran { ok, line, root } => {
                    self.home.notice = Some(line);
                    if ok {
                        self.home.tab = Tab::Watch;
                        self.detail_stale = true;
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
