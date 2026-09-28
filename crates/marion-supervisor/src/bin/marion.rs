//! `marion` — the user-facing command (§10).
//!
//! M1 ships exactly one subcommand, `run`, because §9 requires that the root **not** be
//! hand-started: it is marion that gives the root an `AgentId`, an agent-dir and a per-run token,
//! and without those `TaskContract.requester` has no value to carry.
//!
//! It lives in `marion-supervisor` rather than in a `marion-tui` crate of its own because M1 has
//! no TUI and, more to the point, no socket: §9's "no daemon" means no *detached* supervisor, so
//! `marion run` and `marion-supervisor mcp` must reach the *same* `run_spawn`. §10's table moves
//! the socket at M2; splitting the binary out belongs with that move, not before it, when the
//! split would only buy a second copy of the spawn path with nothing to keep the copies honest.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::Duration as StdDuration;

use marion_core::agent_type::AgentTypes;
use marion_core::contract::ExitStatus;
use marion_core::paths::state_dir_from_env;
use marion_core::production_native_facades;
use marion_supervisor::detach;
use marion_supervisor::duplex::StreamEvent;
use marion_supervisor::facade_cli::dispatch_native_facade_or_legacy;
use marion_supervisor::root;
use marion_supervisor::socket;
use marion_supervisor::watch::{ChildEvent, JournalWatch};
use serde_json::Value;

use cli::{Backend, Exit, Place, Word, Words};

#[path = "marion/cli.rs"]
mod cli;

/// The tool names `marion mcp` declares in `tools/list`, read off the answer it gives so `--help`
/// cannot drift from it.
fn mcp_tool_names() -> Vec<String> {
    let listed = marion_supervisor::bridge::tools_list_result(
        &Value::Null,
        Ok(&AgentTypes::builtins_only()),
        false,
    );
    listed["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// Bare `marion` with no terminal, or a first word that is an option marion does not have:
/// `marion --help`'s text on stderr, and exit 2.
fn usage() -> ! {
    eprintln!("{}", cli::top_help());
    std::process::exit(2)
}

struct Args {
    agent_type: String,
    prompt: String,
    place: Place,
    backend: Backend,
    model: Option<String>,
    timeout_secs: Option<u64>,
    /// `--no-change-record`: launch without snapshotting the repository, and therefore without
    /// any built-in tool. See `root::RootSpec::no_change_record`.
    no_change_record: bool,
    /// `--pane`: run the root in a terminal marion owns, attachable with `marion attach`. See
    /// `root::RootSpec::pane`.
    pane: bool,
    /// `--detach`: start the root and return, leaving it to the supervisor — the same thing a paned
    /// root always does. What the home screen's Start runs, and what a script that only wants the
    /// node started asks for; `marion ls` and `marion attach` are the view.
    detach: bool,
    /// `--profile <name>`: which of the operator's own logins the root runs on (`profiles.toml`).
    profile: Option<String>,
}

/// `marion attach|cancel <id> [--repo <path>] [--state-dir <path>]`: one agent, and which project.
///
/// Its own small parser, as every command has: the only flags it shares with another are the two
/// that answer "which supervisor", and those come from [`Place`], so an attach resolves the same
/// project the run did rather than asking a different supervisor about a node it never heard of.
struct AttachArgs {
    agent_id: String,
    place: Place,
}

fn parse_attach(argv: &[String]) -> Result<AttachArgs, Exit> {
    let mut words = Words::after_verb(argv);
    let (mut target, mut place) = (None, Place::default());
    while let Some(word) = words.next()? {
        match word {
            Word::Flag(f, v) if place.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(id) if target.is_none() => target = Some(id),
            Word::Plain(extra) => return Err(Exit::Usage(format!("unexpected `{extra}`"))),
        }
    }
    Ok(AttachArgs {
        agent_id: cli::required(target, "an agent <id>")?,
        place,
    })
}

/// The whole of `marion attach`, from argv to exit code.
fn attach_main(argv: &[String]) -> Result<ExitCode, Exit> {
    let args = parse_attach(argv)?;
    let Some((repo, state)) = resolve_project(&args.place) else {
        return Ok(ExitCode::FAILURE);
    };
    // With nobody serving there is no tree to resolve against, and the id goes on as typed so
    // attach's own refusal — which says why it will not start a supervisor — is what is printed.
    let agent_id = match marion_supervisor::tree::snapshot(&repo, &state) {
        Ok(nodes) => match marion_supervisor::tree::resolve_target(&args.agent_id, &nodes) {
            Ok(id) => id.0,
            Err(e) => {
                eprintln!("marion: {e}");
                return Ok(ExitCode::FAILURE);
            }
        },
        Err(_) => args.agent_id.clone(),
    };
    Ok(
        match marion_supervisor::attach::run(&agent_id, &repo, &state) {
            Ok(marion_supervisor::attach::Leave::Ended) => ExitCode::SUCCESS,
            // After the `Screen` guard has restored the terminal, as the refusal below is.
            Ok(marion_supervisor::attach::Leave::Detached) => {
                eprintln!("{}", marion_supervisor::attach::reattach_hint(&agent_id));
                ExitCode::SUCCESS
            }
            Err(e) => {
                // After the `Screen` guard has restored the terminal — `Session::drop` runs before
                // this returns — so the sentence lands on the operator's real screen rather than on
                // an alternate one that is about to disappear.
                eprintln!("marion: {e}");
                ExitCode::FAILURE
            }
        },
    )
}

/// **The agent `target` names in this project's live tree** ([`marion_supervisor::tree::resolve_target`]):
/// a whole id, a short id or a unique start of one, read off one `tree/subscribe` snapshot. Fails
/// with the snapshot's own sentence when nobody is serving, which is the answer every command that
/// uses this would have given anyway.
fn find_node(
    target: &str,
    repo: &Path,
    state: &Path,
) -> Result<marion_core::contract::AgentId, String> {
    marion_supervisor::tree::snapshot(repo, state)
        .and_then(|nodes| marion_supervisor::tree::resolve_target(target, &nodes))
}

/// `marion resume <id> [--prompt <text>] [--canned [--base-url <url>]] [--repo …] [--state-dir …]`.
struct ResumeArgs {
    agent_id: String,
    prompt: String,
    place: Place,
    backend: Backend,
}

fn parse_resume(argv: &[String]) -> Result<ResumeArgs, Exit> {
    let mut words = Words::after_verb(argv);
    let (mut target, mut prompt) = (None, String::new());
    let (mut place, mut backend) = (Place::default(), Backend::default());
    while let Some(word) = words.next()? {
        match word {
            Word::Flag("--prompt", v) => prompt = words.value("--prompt", v)?,
            Word::Flag(f, v) if place.take(f, v, &mut words)? => {}
            Word::Flag(f, v) if backend.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(id) if target.is_none() => target = Some(id),
            Word::Plain(extra) => return Err(Exit::Usage(format!("unexpected `{extra}`"))),
        }
    }
    Ok(ResumeArgs {
        agent_id: cli::required(target, "an agent <id>")?,
        prompt,
        place,
        backend,
    })
}

/// **The whole of `marion resume`, from argv to exit code** (`plan-restart-resume.md` step 7).
///
/// **Why resume may start a supervisor where attach may not.** `attach` refuses to start one on
/// purpose: a supervisor started fresh has no record of the node, so it would answer `not found`
/// about marion rather than about the node, and the operator would be told their live node is gone.
/// Resume is the opposite case *by definition*: the node it brings back is one whose supervisor
/// **died with it**, so there is deliberately no supervisor to dial, and starting one is not a
/// silent fallback — it is the operation. The relaunch reads the node from the on-disk journal the
/// new supervisor boots over, so the node is exactly as re-findable as it was before — and the id
/// is resolved against that same journal, since there is no live tree to ask.
fn resume_main(argv: &[String]) -> Result<ExitCode, Exit> {
    let args = parse_resume(argv)?;
    Ok(match resume(&args) {
        Ok(code) | Err(code) => code,
    })
}

/// The resume's stages in order: resolve, dial, `node/resume`, announce, then [`root_follow`]'s
/// rule — watch a headless node to its exit as `run` does, or say a paned one started. Each
/// refusal has already printed its sentence and carries only the code, as [`run`]'s do.
fn resume(args: &ResumeArgs) -> Result<ExitCode, ExitCode> {
    let (repo, state) = resolve_project(&args.place).ok_or(ExitCode::FAILURE)?;
    let base_url = endpoint(&args.backend)?;
    let project_key = socket::project_root(&repo);
    let project = marion_core::paths::ProjectDir::new(&state, &project_key);
    let agent_id = journal_node(&args.agent_id, &project.journal()).map_err(|e| {
        eprintln!("marion: {e}");
        ExitCode::FAILURE
    })?;
    let sock = socket::socket_paths(&state, &project_key, socket::own_uid());
    let launch = detach::Launch {
        program: supervisor_binary(),
        state_dir: state.clone(),
        project_root: project_key.clone(),
        idle_grace: RUN_IDLE_GRACE,
        auth: args.backend.auth(),
        base_url,
    };
    let (mut session, started) = connect_supervisor(&sock, &launch)?;
    let terminal = std::sync::Arc::new(Terminal::new(io::stderr()));
    // The same journal tail `run` starts before its spawn, for the same reason: measured before
    // the relaunch, so the second life's children are the only news it can report.
    let stop = std::sync::Arc::new(marion_supervisor::wake::Flag::new());
    let (poller, poller_id) = tail_children(project.journal(), &terminal, &stop);
    let resumed = match resume_node(&mut session, &agent_id, &args.prompt) {
        Ok(r) => r,
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            drop(poller_id);
            let _ = poller.join();
            return Err(resume_refused(&mut session, started, &e));
        }
    };
    let root_id = resumed.agent_id.clone();
    let _ = poller_id.send(root_id.clone());
    eprintln!(
        "marion: resumed {} (generation {})",
        root_id.0, resumed.spawn_generation
    );
    // **From here the resume is a run whose root already exists**, and it follows [`root_follow`]'s
    // one rule rather than a second one of its own. This used to hand every resumed node to
    // `attach::run`, which is the *paned* answer: on the headless node every resume actually
    // produces (a pane names no session, so only headless nodes are resumable) the attach was
    // refused — *"no display plane"* — and the command exited 1 while the node ran on, having
    // detached from it on the way out. The shape is read off the node's journal, which is the
    // record the supervisor rebuilt the relaunch from.
    let shape = resumed_shape(&project.journal(), &root_id);
    match root_follow(shape.pane) {
        RootFollow::Started => {
            stop.store(true, Ordering::Relaxed);
            let _ = poller.join();
            eprintln!("{}", pane_started_line(&root_id, &shape.agent_type));
            Ok(ExitCode::SUCCESS)
        }
        RootFollow::Watch => {
            let watched = watch_the_root(&mut session, &root_id, &terminal);
            stop.store(true, Ordering::Relaxed);
            let _ = poller.join();
            report_watched(watched, &project.journal(), &root_id, shape.bound)
        }
    }
}

/// **The agent `target` names in this project's journal**, for a command that must work with no
/// supervisor running — [`marion_supervisor::tree::resolve_node`] over every node the journal
/// records. A journal that cannot be read resolves nothing, and `target` goes on as typed.
fn journal_node(target: &str, journal: &Path) -> Result<marion_core::contract::AgentId, String> {
    let mut replay = marion_core::registry::Replay::default();
    replay.extend(&std::fs::read(journal).unwrap_or_default());
    marion_supervisor::tree::resolve_node(
        target,
        replay.nodes().iter().map(|n| n.agent_id.0.as_str()),
    )
}

/// The two facts about a resumed node the client needs after the relaunch and the supervisor's
/// answer does not carry: the shape it was relaunched in, and the clock it runs under.
struct ResumedShape {
    /// `true` for a pane, `false` headless — [`marion_core::registry::ReplayedNode::harness_pane`],
    /// the value the supervisor relaunched from. Headless where the journal says nothing, which
    /// is also what replay says.
    pane: bool,
    /// The node's own bound, resolved as `run` resolves it: the recorded one, else the type's.
    bound: StdDuration,
    /// For the pane line, which names the type as `run`'s does.
    agent_type: String,
}

/// Read a resumed node's [`ResumedShape`] off the project journal.
///
/// Best effort by construction, as [`root_denials`] is: a journal that cannot be read yields the
/// defaults — headless, the type's bound — because the node exists and is being watched either
/// way, and a viewer's trouble reading a file must not turn a resumed run into a refusal.
fn resumed_shape(journal: &Path, agent_id: &marion_core::contract::AgentId) -> ResumedShape {
    let bytes = std::fs::read(journal).unwrap_or_default();
    let replay = marion_core::registry::replay(&bytes);
    let node = replay.get(agent_id);
    let intent = node.and_then(|n| n.intent.as_ref());
    let agent_type = intent.map(|i| i.agent_type.clone()).unwrap_or_default();
    let type_secs = marion_core::agent_type::builtin(&agent_type)
        .map(|t| t.timeout.0.as_secs())
        .unwrap_or(marion_supervisor::handler::DEFAULT_SPAWN_TIMEOUT_SECS);
    ResumedShape {
        pane: node.is_some_and(|n| n.harness_pane),
        bound: StdDuration::from_secs(blocked_bound_secs(
            intent.and_then(|i| i.timeout_secs),
            type_secs,
        )),
        agent_type,
    }
}

/// The endpoint a `run` or `resume` points at, by [`resolve_reachable_base_url`]'s rule; a refusal
/// is its sentence and the code.
fn endpoint(backend: &Backend) -> Result<Option<String>, ExitCode> {
    resolve_reachable_base_url(
        backend.canned,
        backend.base_url.clone(),
        std::env::var("MARION_BASE_URL").ok(),
    )
    .map_err(|e| {
        eprintln!("marion: {e}");
        ExitCode::FAILURE
    })
}

/// Send `node/resume` and read its answer. The error is the whole sentence to print, `marion: `
/// prefix included.
fn resume_node(
    session: &mut SupervisorSession,
    agent_id: &marion_core::contract::AgentId,
    prompt: &str,
) -> Result<marion_core::proto::result::NodeResumeResult, String> {
    let id = session.send(marion_core::proto::Call::NodeResume(
        marion_core::proto::params::NodeResumeParams {
            agent_id: agent_id.clone(),
            prompt: prompt.to_string(),
        },
    ))?;
    match session.pump(Awaited::Response(id), &mut |_| {})? {
        marion_core::proto::Outcome::Result(body) => {
            match marion_core::proto::Method::NodeResume.decode_result(&body) {
                Ok(marion_core::proto::MethodResult::NodeResume(r)) => Ok(r),
                _ => Err(
                    "marion: this project's supervisor answered `node/resume` with a \
                              result marion cannot read"
                        .to_string(),
                ),
            }
        }
        marion_core::proto::Outcome::Error(e) => Err(format!("marion: {}", e.message)),
    }
}

/// Print a refused resume's sentence and, when this command is what started the supervisor, end
/// it. Always the failure code.
fn resume_refused(session: &mut SupervisorSession, started: bool, reason: &str) -> ExitCode {
    eprintln!("{reason}");
    // **If this command is what started the supervisor, it is what ends it.** A resume that
    // could not happen must not leave a supervisor an operator did not have before — the
    // node it was for is still gone, and an empty supervisor lingering out its idle grace
    // is a surprise, not a service. A supervisor that was already serving is left alone.
    if started {
        let _ = session.send(marion_core::proto::Call::SessionQuit(
            marion_core::proto::params::SessionQuitParams {
                disposition: marion_core::proto::QuitDisposition::KillTree { confirmed: vec![] },
            },
        ));
    }
    ExitCode::FAILURE
}

/// `marion ls [<id>] [--attention] [--repo <path>] [--state-dir <path>]` — and `marion list`,
/// which is the same command printed as lines.
#[derive(Debug, Default, PartialEq, Eq)]
struct ListArgs {
    target: Option<String>,
    /// Keep only the nodes `tree::attention_of` names; a filter is text, so it prints lines.
    attention: bool,
    place: Place,
}

fn parse_list(argv: &[String]) -> Result<ListArgs, Exit> {
    let mut words = Words::after_verb(argv);
    let mut args = ListArgs::default();
    while let Some(word) = words.next()? {
        match word {
            Word::Flag("--attention", v) => args.attention = cli::switch("--attention", v)?,
            Word::Flag(f, v) if args.place.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(id) if args.target.is_none() => args.target = Some(id.to_string()),
            Word::Plain(extra) => return Err(Exit::Usage(format!("unexpected `{extra}`"))),
        }
    }
    Ok(args)
}

/// **The whole of `marion ls`.** With an agent: that agent's detail as text (what marion sent it,
/// its messages, its tokens, where it works and how it ended), which is what the Watch tab's
/// expansion shows. Without one, on a terminal: the home screen on its Watch tab. Without one and
/// with no terminal — a pipe, a script — or with `--attention`: the forest one node per line,
/// because there is nobody to press a key or the operator asked for a filtered list. It starts no
/// supervisor.
fn ls_main(argv: &[String]) -> Result<ExitCode, Exit> {
    let args = parse_list(argv)?;
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    Ok(ls(args, interactive))
}

/// `marion list`: [`ls_main`], always as lines.
fn list_main(argv: &[String]) -> Result<ExitCode, Exit> {
    Ok(ls(parse_list(argv)?, false))
}

fn ls(args: ListArgs, interactive: bool) -> ExitCode {
    let Some((repo, state)) = resolve_project(&args.place) else {
        return ExitCode::FAILURE;
    };
    match args.target {
        Some(id) => ls_one(&id, &repo, &state),
        None if interactive && !args.attention => {
            home_on(marion_tui::home::Tab::Watch, repo, state)
        }
        None => list_lines(&repo, &state, args.attention),
    }
}

/// Bare `marion`: the home screen on Start, for the repository of the working directory.
fn home_main() -> ExitCode {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        usage()
    }
    let Some((repo, state)) = resolve_project(&Place::default()) else {
        return ExitCode::FAILURE;
    };
    home_on(marion_tui::home::Tab::Start, repo, state)
}

fn home_on(tab: marion_tui::home::Tab, repo: PathBuf, state: PathBuf) -> ExitCode {
    let opts = marion_supervisor::home::session::Options { repo, state, tab };
    match marion_supervisor::home::session::run(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("marion: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `marion ls <id>`: one node's detail, as text.
fn ls_one(target: &str, repo: &Path, state: &Path) -> ExitCode {
    use marion_supervisor::tree;
    let found = tree::snapshot(repo, state).and_then(|nodes| {
        let id = tree::resolve_target(target, &nodes)?;
        let node = nodes
            .iter()
            .find(|n| n.agent_id == id)
            .cloned()
            .ok_or_else(|| format!("no node `{target}` in this project's forest"))?;
        let sock = socket::socket_paths(state, &socket::project_root(repo), socket::own_uid());
        let got = marion_supervisor::courier::node_get_with(
            sock.socket(),
            &id,
            Some(marion_core::proto::params::ActivityCursor::Tail),
        )
        .map_err(|e| e.to_string())?;
        Ok((node, got.detail))
    });
    match found {
        Ok((node, detail)) => {
            print!("{}", detail_text(&node, &detail));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("marion: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A node and its detail as the lines `marion ls <id>` prints. Tokens only, never a price.
fn detail_text(
    node: &marion_core::proto::NodeSummary,
    d: &marion_core::proto::result::NodeDetail,
) -> String {
    use marion_core::contract::Workspace;
    let mut out = format!(
        "{}
",
        marion_supervisor::tree::list_line(node)
    );
    let mut kv = |k: &str, v: &str| {
        out.push_str(&format!(
            "  {k:<10} {v}
"
        ))
    };
    if let Some(t) = &d.task {
        for (i, line) in t.prompt.lines().enumerate() {
            kv(if i == 0 { "task" } else { "" }, line);
        }
        for a in &t.acceptance {
            kv("accept", a);
        }
        for v in &t.verification {
            kv("verify", v);
        }
    }
    for m in &d.messages {
        kv(
            "message",
            &format!("{} {} · {} bytes · {}", m.at, m.from, m.len, m.outcome),
        );
    }
    if let Some(u) = d.usage {
        kv(
            "tokens",
            &format!(
                "{} in · {} out · {} cached",
                u.input, u.output, u.cache_read
            ),
        );
    }
    if let Some(w) = &d.workspace {
        let (path, branch) = match w {
            Workspace::Worktree { path, branch } => (path, Some(branch)),
            Workspace::SharedCwd { path } => (path, None),
        };
        kv("workspace", &path.display().to_string());
        if let Some(b) = branch {
            kv("branch", b);
        }
    }
    if let Some(c) = &d.completion {
        kv("result", c.status.word());
        if let Some(n) = &c.narrative {
            kv("said", n.lines().next().unwrap_or(""));
        }
        if let Some(b) = &c.branch {
            kv("merge", &format!("git merge --no-ff {b}"));
        }
    }
    if let Some(page) = &d.stream {
        if let Some(why) = &page.unread {
            kv("activity", why);
        }
        for l in &page.lines {
            kv(
                "",
                &format!("{}  {}", l.at.get(11..19).unwrap_or(&l.at), l.text),
            );
        }
    }
    out
}

/// The forest, one [`marion_supervisor::tree::list_line`] per node, filtered to the nodes that
/// need the operator under `attention`: `marion list`, and `marion ls` without a terminal.
fn list_lines(repo: &Path, state: &Path, attention: bool) -> ExitCode {
    use marion_supervisor::tree;
    let nodes = match tree::snapshot(repo, state) {
        Ok(nodes) => nodes,
        Err(e) => {
            eprintln!("marion: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = io::stdout().lock();
    for node in nodes
        .iter()
        .filter(|n| !attention || tree::attention_of(n).is_some())
    {
        if writeln!(out, "{}", tree::list_line(node)).is_err() {
            // A closed stdout — `marion list | head -1` — is the reader's decision, not a failure.
            return ExitCode::SUCCESS;
        }
    }
    ExitCode::SUCCESS
}

/// `marion steer <id> [--repo <path>] [--state-dir <path>] [--] <text…>`.
struct SteerArgs {
    /// A whole agent id, a short id or a unique start of one ([`find_node`]).
    target: String,
    text: SteerText,
    place: Place,
}

/// Where the message comes from.
#[derive(Debug, PartialEq, Eq)]
enum SteerText {
    /// The words after the flags, joined with single spaces.
    Words(String),
    /// A lone `-`: stdin, read to its end.
    Stdin,
}

/// The project flags come only **before** the message and `--` ends them, so a message may say
/// anything — including `--repo` or `--help` — without being read as a flag; an unknown flag before
/// the message is a refusal rather than the start of it, because a mistyped `--state-dir` taken as
/// text would steer the right node with the wrong words.
fn parse_steer(argv: &[String]) -> Result<SteerArgs, Exit> {
    let mut words = Words::after_verb(argv);
    let (mut target, mut place, mut first) = (None, Place::default(), None);
    while let Some(word) = words.next()? {
        match word {
            Word::Flag(f, v) if place.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(id) if target.is_none() => target = Some(id),
            Word::Plain(w) => {
                first = Some(w);
                break;
            }
        }
    }
    let target = cli::required(target, "an agent <id> and a message")?;
    let mut message: Vec<&str> = first.into_iter().collect();
    message.extend(words.rest());
    let text = match message.as_slice() {
        [] => return Err(Exit::Usage("needs a message after the <id>".into())),
        ["-"] => SteerText::Stdin,
        ["-", ..] => {
            return Err(Exit::Usage(
                "`-` reads the message from stdin and takes nothing after it".into(),
            ));
        }
        _ => SteerText::Words(message.join(" ")),
    };
    Ok(SteerArgs {
        target,
        text,
        place,
    })
}

/// A piped message without the one line ending the pipe added: `echo x | marion steer id -` means
/// `x`, not `x` and a newline. Only one, and nothing else is trimmed — the rest is the message.
fn piped_message(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    text
}

/// **The whole of `marion steer`**: resolve the target against one `tree/subscribe` snapshot, send
/// `node/steer` as the operator (`caller: None`), and print what the supervisor said.
///
/// Exit 0 when the message was queued, printing where it goes; exit 1 with the supervisor's own
/// sentence when it was refused. Like `ls` it starts no supervisor: a supervisor started here
/// would have no node to steer, and "unknown node" would be a lie about the operator's agents.
fn steer_main(argv: &[String]) -> Result<ExitCode, Exit> {
    let args = parse_steer(argv)?;
    let text = match args.text {
        SteerText::Words(w) => w,
        SteerText::Stdin => {
            let mut buf = String::new();
            if let Err(e) = io::Read::read_to_string(&mut io::stdin(), &mut buf) {
                eprintln!("marion: reading the message from stdin: {e}");
                return Ok(ExitCode::FAILURE);
            }
            piped_message(buf)
        }
    };
    if text.trim().is_empty() {
        eprintln!("marion: the message is empty, so there is nothing to steer with");
        return Ok(ExitCode::FAILURE);
    }
    let Some((repo, state)) = resolve_project(&args.place) else {
        return Ok(ExitCode::FAILURE);
    };
    let steered = find_node(&args.target, &repo, &state).and_then(|agent_id| {
        let sock = socket::socket_paths(&state, &socket::project_root(&repo), socket::own_uid());
        marion_supervisor::courier::steer(sock.socket(), &agent_id, &text, None)
            .map_err(|e| e.to_string())
    });
    Ok(match steered {
        Ok(s) => {
            println!("marion: {}", s.sentence());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("marion: {e}");
            ExitCode::FAILURE
        }
    })
}

/// **The whole of `marion cancel`**: resolve the target against one snapshot, send `node/kill` as
/// the operator, and print what happened.
///
/// Exit 0 with the node's new state; exit 1 with the supervisor's own sentence when it refused —
/// a node already ended, one still spawning with nothing to signal. Like `steer` it starts no
/// supervisor, for `steer`'s reason. It does not ask for confirmation: a command typed at a shell
/// is the confirmation, and the home screen asks before it runs this.
fn cancel_main(argv: &[String]) -> Result<ExitCode, Exit> {
    let args = parse_attach(argv)?;
    let Some((repo, state)) = resolve_project(&args.place) else {
        return Ok(ExitCode::FAILURE);
    };
    let killed = find_node(&args.agent_id, &repo, &state).and_then(|agent_id| {
        let sock = socket::socket_paths(&state, &socket::project_root(&repo), socket::own_uid());
        marion_supervisor::courier::kill(sock.socket(), &agent_id)
            .map(|r| (agent_id, r))
            .map_err(|e| e.to_string())
    });
    Ok(match killed {
        Ok((id, r)) => {
            let state = match r.state {
                marion_core::node::NodeState::Exited(s) => s.word().to_string(),
                other => format!("{other:?}").to_lowercase(),
            };
            println!(
                "marion: {} ended: {state}",
                marion_supervisor::tree::short_id(&id.0)
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("marion: {e}");
            ExitCode::FAILURE
        }
    })
}

/// **Which project a command means**: [`Place`]'s two flags, resolved once for every command.
///
/// The pair is a *repo* and a *state dir* resolved together because §2 keys the socket on both,
/// and two copies of that resolution is exactly how one command ends up dialling a different
/// supervisor than another.
fn resolve_project(place: &Place) -> Option<(PathBuf, PathBuf)> {
    let repo = place.repo.clone().unwrap_or_else(|| {
        default_repo(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    });
    let repo = match repo.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("marion: cannot resolve repo {}: {e}", repo.display());
            return None;
        }
    };
    let state = state_dir_or_report(place.state_dir.as_deref())?;
    Some((repo, state))
}

/// **The one place the CLI resolves a state directory**, for every verb: `--state-dir`, else
/// [`state_dir_from_env`]'s rule, which the native facade (`socket::resolve_state_dir`) reads too,
/// so `marion claude` and `marion ls` in one shell find the same supervisor.
fn state_dir_or_report(explicit: Option<&str>) -> Option<std::path::PathBuf> {
    let state = state_dir_from_env(explicit);
    if state.is_none() {
        eprintln!(
            "marion: cannot resolve a state directory (set --state-dir, $MARION_STATE_DIR or $HOME)"
        );
    }
    state
}

/// `run`'s words. **An unknown flag is a refusal, never a silent ignore**: a mistyped
/// `--state-dir` that fell through would write the run's state under `$HOME` and leave the
/// operator hunting for a contract that is not where they looked.
fn parse_args(argv: &[String]) -> Result<Args, Exit> {
    let mut words = Words::after_verb(argv);
    let mut agent_type = None;
    let mut args = Args {
        agent_type: String::new(),
        prompt: String::new(),
        place: Place::default(),
        backend: Backend::default(),
        model: None,
        timeout_secs: None,
        no_change_record: false,
        pane: false,
        detach: false,
        profile: None,
    };
    while let Some(word) = words.next()? {
        match word {
            Word::Flag(f, v) if apply_run_flag(&mut args, f, v, &mut words)? => {}
            Word::Flag(f, v) if args.place.take(f, v, &mut words)? => {}
            Word::Flag(f, v) if args.backend.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(t) if agent_type.is_none() => agent_type = Some(t),
            Word::Plain(extra) => {
                return Err(Exit::Usage(format!(
                    "unexpected `{extra}`; the task goes in --prompt"
                )));
            }
        }
    }
    args.agent_type = cli::required(agent_type, "an agent type, e.g. `marion run claude`")?;
    // A root with no turn does nothing, so there is nothing to start without one.
    if args.prompt.is_empty() {
        return Err(Exit::Usage("needs --prompt <text>, the task".into()));
    }
    Ok(args)
}

/// One of `run`'s own flags, taking its value from `words` when it has one. `false` when `flag` is
/// not one of them.
fn apply_run_flag(
    args: &mut Args,
    flag: &str,
    inline: Option<&str>,
    words: &mut Words,
) -> Result<bool, Exit> {
    match flag {
        // Valueless, and deliberately so: declining the audit that a grant is conditional on is
        // a decision, and it should read as one at the call site.
        "--no-change-record" => args.no_change_record = cli::switch(flag, inline)?,
        "--pane" => args.pane = cli::switch(flag, inline)?,
        "--detach" => args.detach = cli::switch(flag, inline)?,
        "--prompt" => args.prompt = words.value(flag, inline)?,
        "--model" => args.model = Some(words.value(flag, inline)?),
        "--profile" => args.profile = Some(words.value(flag, inline)?),
        "--timeout" => {
            let secs = words.value(flag, inline)?;
            args.timeout_secs = Some(secs.parse().map_err(|_| {
                Exit::Usage(format!("--timeout takes whole seconds, not `{secs}`"))
            })?);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// §9: "`marion run --timeout`, else its agent type's `timeout_secs`, else the same 900 s".
///
/// **The resolution moved to `root::blocked_bound_secs` and this is the same call.** Since §11 item
/// 28 step 6 the supervisor launches the root, so the bound the node runs under is resolved there;
/// what `marion run` still needs it for is the *sentence* it prints when a root is killed on that
/// bound, which has to name the number the node actually ran under. Re-deriving it here with a
/// second copy of the rule is exactly how those two numbers would come to disagree.
fn blocked_bound_secs(explicit: Option<u64>, agent_type_secs: u64) -> u64 {
    root::blocked_bound_secs(explicit, agent_type_secs)
}

/// marion's own canned provider, where a run points under `--canned`.
const CANNED_BASE_URL: &str = "http://127.0.0.1:8099/v1";

/// Whether a base URL names a host on this machine's loopback interface.
///
/// Hand-rolled rather than parsed, because marion has no URL dependency and the question is
/// narrower than parsing: *is the authority a loopback name*. It takes the authority — after the
/// scheme, before the first `/`, `?` or `#` — drops any `user:pass@`, unwraps an IPv6 literal's
/// brackets, drops the port, and compares. `127.0.0.0/8` is matched as a range, because
/// `http://127.0.0.2:8099` is quite as loopback as `127.0.0.1` and a string equality would miss it.
///
/// It errs toward *saying yes*: a false positive refuses a live run that would have worked and
/// costs the operator a flag, while a false negative sends a real credential to a fake server.
fn is_loopback_url(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host_port.strip_prefix('[') {
        // An IPv6 literal: everything up to the closing bracket, port and all left outside.
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => host_port.rsplit_once(':').map_or(host_port, |(h, _)| h),
    };
    let host = host.trim().to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "::1"
        || host == "0:0:0:0:0:0:0:1"
        || host
            .strip_prefix("127.")
            .is_some_and(|_| host.split('.').count() == 4)
}

/// The endpoint a run points at, and the safety gate on the one combination that must not exist.
///
/// By default the answer is **no endpoint at all**: each harness resolves the vendor it is already
/// logged in to, which is the whole premise — marion overlays nothing rather than pointing the node
/// somewhere. `$MARION_BASE_URL` is ignored there for that reason; it names marion's canned server,
/// and inheriting it silently is precisely the accident below. Under `--canned` it is the flag,
/// then the environment, then marion's own provider.
///
/// **`--base-url` without `--canned` is refused, in two flavours.** Both are errors rather than
/// precedence rules, and they are kept apart because the operator's diagnosis differs even though
/// the remedy does not:
///
/// 1. **A loopback endpoint** is the one combination that aims a real credential at a fake server:
///    the operator's own key or OAuth token, sent in the clear to whatever is listening on that
///    port. Every other flag conflict here costs a run; this one can cost a credential.
/// 2. **Any other endpoint** — a proxy or a gateway — is refused because *marion does not implement
///    it*. This is the §12 accept-and-ignore shape, and until this refusal landed marion was the
///    one producing it: `root::compile` copied the URL into `LaunchSpec.base_url` and **every
///    adapter then dropped it** under [`Auth::Inherited`] (claude's `(None, None)` arm, gemini's
///    `GOOGLE_GEMINI_BASE_URL` push gated on `Canned`, codex's and opencode's `config_files` early
///    returns). So an operator pointed marion at a corporate gateway, got no error, and the node
///    reached the vendor directly with a real credential — traffic leaving the perimeter the
///    gateway existed to hold, logged nowhere they could see. `tests/auth_mode.rs` pins each
///    adapter's drop, which is now defence in depth behind this gate.
///
/// **Refusing rather than implementing is the deliberate, reversible direction**, exactly as with
/// `isolation: "remote"` in `spawn::SpawnError`. Honouring a gateway is a real feature —
/// LiteLLM and corporate proxies are ordinary — and it can land later against this refusal. What
/// cannot be undone is teaching an operator that marion silently reaches the vendor: the missing
/// capability is merely absent, while advertising one that does not exist is the bug. The usage
/// text is part of that promise and is corrected alongside this.
///
/// **`--canned` is untouched.** Endpoint, environment, then marion's own provider — that is the
/// whole test suite's launch path, and it is the half a careless refusal breaks.
///
/// By default the answer is **no endpoint at all**: each harness resolves the vendor it is already
/// logged in to, which is the whole premise — marion overlays nothing rather than pointing the node
/// somewhere. `$MARION_BASE_URL` is ignored there for that reason; it names marion's canned server,
/// and inheriting it silently is precisely the accident case 1 refuses loudly.
fn resolve_base_url(
    canned: bool,
    explicit: Option<String>,
    from_env: Option<String>,
) -> Result<Option<String>, String> {
    match (canned, explicit) {
        (false, Some(u)) if is_loopback_url(&u) => Err(format!(
            "a loopback --base-url ({u}) without --canned is refused: marion runs the node \
             against the vendor the operator is logged in to, so it presents a real credential, \
             and a loopback endpoint is marion's canned provider or some other local process — \
             the combination sends a real credential to a fake server. Drop --base-url to let the \
             harness reach its own vendor, or pass --canned to run against the canned endpoint."
        )),
        (false, Some(u)) => Err(format!(
            "--base-url ({u}) without --canned is not implemented — marion accepts no endpoint for \
             a node that presents the operator's own login. Every adapter drops it in that mode \
             (claude emits no ANTHROPIC_BASE_URL, gemini no GOOGLE_GEMINI_BASE_URL, codex no \
             model_providers entry, opencode no provider block), so the run would have reached the \
             vendor directly while looking like it honoured the gateway. Drop --base-url to let \
             the harness reach its own vendor, or pass --canned to run against marion's canned \
             endpoint. Pointing a real credential through a proxy is a reasonable thing to want; \
             marion will not pretend to do it until it does."
        )),
        (false, None) => Ok(None),
        (true, explicit) => Ok(Some(
            explicit
                .or(from_env)
                .unwrap_or_else(|| CANNED_BASE_URL.into()),
        )),
    }
}

/// How long `--canned` waits for its provider to accept a connection before refusing the run.
const CANNED_PROBE: StdDuration = StdDuration::from_secs(1);

/// **Is anything listening at the canned endpoint?** A bounded TCP connect, nothing more.
///
/// Measured: `marion run codex --prompt "say hello" --canned` with no provider on :8099 hung
/// forever — the harness retries a refused connection without end (opencode never exits on one at
/// all, S13) — and left the root and its supervisor running after the client was killed. A connect
/// probe turns that into one sentence naming the command that starts the provider. It proves only
/// that a port is open, not that the listener is marion's provider; that is the operator's to say.
fn canned_endpoint_listening(url: &str) -> Result<(), String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let not_listening = |why: &str| {
        format!(
            "--canned points at {url}, and {why}. Start marion's canned provider first, from a \
             marion checkout: `cargo install --path crates/marion-provider` and then \
             `marion-canned &` (MARION_CANNED_PORT picks the port, default 8099), or pass \
             --base-url for one that is already running. Drop --canned to run on your own login."
        )
    };
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| not_listening("that is not an http:// URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let has_port = match authority.strip_prefix('[') {
        Some(v6) => v6.contains("]:"),
        None => authority.contains(':'),
    };
    let target = match (has_port, scheme) {
        (true, _) => authority.to_string(),
        (false, "https") => format!("{authority}:443"),
        (false, _) => format!("{authority}:80"),
    };
    let addrs: Vec<_> = target
        .to_socket_addrs()
        .map_err(|e| not_listening(&format!("its address does not resolve ({e})")))?
        .collect();
    if addrs
        .iter()
        .any(|a| TcpStream::connect_timeout(a, CANNED_PROBE).is_ok())
    {
        return Ok(());
    }
    Err(not_listening("nothing is listening there"))
}

/// [`resolve_base_url`], then [`canned_endpoint_listening`] on the endpoint it chose: one refusal
/// path for `run`, `resume` and `mcp`, so none of them can hang on a provider that is not there.
fn resolve_reachable_base_url(
    canned: bool,
    explicit: Option<String>,
    from_env: Option<String>,
) -> Result<Option<String>, String> {
    let url = resolve_base_url(canned, explicit, from_env)?;
    if let Some(u) = &url {
        canned_endpoint_listening(u)?;
    }
    Ok(url)
}

/// `--repo`'s default: the enclosing git repository, else the working directory itself.
///
/// A `.git` **entry**, not a `.git` directory: a linked worktree and a submodule both carry a
/// `.git` *file*, and treating those as "not a repository" would silently scope a node to a
/// subdirectory of the checkout the operator is standing in.
fn git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn default_repo(cwd: &Path) -> PathBuf {
    git_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

/// `marion mcp`'s flags — the two groups of `marion run`'s that say *which project and which
/// provider*, and none of the ones that describe a run.
///
/// There is no `--prompt`, no `--model`, no `--timeout` and no `--no-change-record`, because this
/// command does not perform a run: it serves a tool the client's model calls, and every one of
/// those is a field of the `spawn` call rather than of the server. Accepting them here would be a
/// second place a run's shape is decided, silently losing to the tool call whenever the two
/// disagreed.
#[derive(Debug, PartialEq, Eq, Default)]
struct McpArgs {
    place: Place,
    backend: Backend,
}

/// **An unknown flag is a refusal, never a silent ignore** — and it matters more here than
/// anywhere, because these flags arrive out of an MCP client's config file where nobody is watching
/// a terminal: a mistyped `--state-dir` that fell through would serve a different project's fleet
/// than the one the operator wrote down, and answer every call successfully.
fn parse_mcp_args(argv: &[String]) -> Result<McpArgs, Exit> {
    let mut words = Words::after_verb(argv);
    let mut args = McpArgs::default();
    while let Some(word) = words.next()? {
        match word {
            Word::Flag(f, v) if args.place.take(f, v, &mut words)? => {}
            Word::Flag(f, v) if args.backend.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(extra) => return Err(Exit::Usage(format!("unexpected `{extra}`"))),
        }
    }
    Ok(args)
}

/// `marion mcp`. **It never falls through to another parser**: it speaks JSON-RPC on stdout from
/// its first line, and a refusal goes to stderr as [`cli::dispatch`] prints every refusal.
fn mcp_main(argv: &[String]) -> Result<ExitCode, Exit> {
    Ok(run_mcp(parse_mcp_args(argv)?))
}

/// **`marion mcp` — marion as an MCP server, for a client marion did not start.**
///
/// §10's `marion` is the user-facing command and §5.4's control MCP is how an agent addresses
/// marion; until now the second was only ever reachable *from inside* a node marion had spawned.
/// This is the same surface pointed the other way: an external MCP client — Claude Code's
/// `.mcp.json`, another agent, an editor — gets `spawn`, `wait`, `status` and `list`, and its
/// `spawn` creates a **root**.
///
/// **It is a socket client and it is not privileged**, which is the whole of why it may exist at
/// all. Every frame it answers is composed out of §2's fifteen methods over the same socket
/// `marion run` dials; it starts no process; and `mcp::Principal` is where the one difference
/// between it and a per-child bridge is written down. See
/// `mcp::tests::the_mcp_entry_point_has_no_spawn_path_of_its_own` for the rule and
/// `mcp::Principal::ensure_supervisor` for the one thing the two surfaces genuinely decide
/// differently.
///
/// **Everything it resolves, it resolves exactly as `marion run` does**, and from the same
/// functions rather than from copies: `default_repo`, `state_dir`, `socket::project_root`,
/// `socket::socket_paths`, `resolve_base_url`. A `marion mcp` and a `marion run` given the same
/// `--repo` must reach the same supervisor over the same journal, and two derivations that agree
/// today are two derivations that can stop agreeing.
///
/// **Nothing but JSON-RPC reaches stdout.** stdout is the protocol here — an MCP client parses
/// every line of it — so each refusal below goes to stderr and ends the process, rather than
/// printing something the client would try to read as a frame.
fn run_mcp(args: McpArgs) -> ExitCode {
    let Some((repo, state)) = resolve_project(&args.place) else {
        return ExitCode::FAILURE;
    };
    // Not probed for a listener, unlike `run`'s: this server may outlive a canned provider that
    // starts after it, and each spawn is refused on its own if nothing answers.
    let base_url = match resolve_base_url(
        args.backend.canned,
        args.backend.base_url.clone(),
        std::env::var("MARION_BASE_URL").ok(),
    ) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("marion: {e}");
            return ExitCode::FAILURE;
        }
    };
    let program = supervisor_binary();
    // §2's key — the git common dir — for the socket and for §4.3's tree both, which is the pairing
    // the run path documents at length. One `project_root`, spent twice.
    let project_key = socket::project_root(&repo);
    let sock = socket::socket_paths(&state, &project_key, socket::own_uid());
    let project = marion_core::paths::ProjectDir::new(&state, &project_key);
    let auth = args.backend.auth();
    marion_supervisor::mcp::serve_stdio(marion_supervisor::mcp::Principal::TopLevel(Box::new(
        marion_supervisor::mcp::TopLevel::new(
            sock,
            project,
            repo,
            detach::Launch {
                program,
                state_dir: state,
                project_root: project_key,
                // §5.7's own default, for the reason the run path gives: whichever client starts a
                // supervisor fixes the grace for every later one, so this is not a number a
                // resident MCP server gets to choose on everybody else's behalf.
                idle_grace: RUN_IDLE_GRACE,
                auth,
                base_url,
            },
        ),
    )));
    ExitCode::SUCCESS
}

// --- the live view ------------------------------------------------------------------------------
//
// A run is minutes long and, until this existed, produced not one byte until it was over: a person
// watching a root delegate to a codex child had a blank terminal and no way to tell a working run
// from a wedged one. What follows turns the frames `duplex` reads into lines a person can read
// **while they are still arriving**.
//
// Three rules it holds to, all of them things the accumulated transcript did badly or not at all:
//
// 1. **No frame is silent.** A kind this renderer has no special handling for still produces one
//    line naming it. Silence on an unhandled kind is the failure family this repo has spent the
//    session removing: it is indistinguishable from nothing having happened.
// 2. **No raw JSON.** A 30 kB `initialize` `control_response` and a 2 kB `tool_result` are both
//    frames; neither is something to paste at a person. Values are summarised and truncated —
//    except an error, which is printed verbatim, because a truncated error is a lie about what the
//    node said.
// 3. **This is the `marion run` path only.** Nothing here is reachable from a child; see
//    `duplex::DuplexSpec::sink`.

/// The width of the tag column. Wide enough for the longest tag below, so the bodies line up and
/// the eye can scan the left edge for the interesting event.
const TAG_WIDTH: usize = 8;

/// How much of one summarised value is shown before it is cut.
const VALUE_CHARS: usize = 72;
/// How much of one summarised line is shown before it is cut.
const LINE_CHARS: usize = 200;

/// `s` truncated to `n` **characters** with an ellipsis, or `s` if it already fits.
///
/// Characters and not bytes: a node's text is arbitrary UTF-8 and slicing it by byte index panics
/// on the first multibyte character, which would take the whole run down to render a line.
fn brief(s: &str, n: usize) -> String {
    let s = s.replace(['\n', '\r', '\t'], " ");
    if s.chars().count() <= n {
        return s;
    }
    let kept: String = s.chars().take(n).collect();
    format!("{}…", kept.trim_end())
}

/// One tagged line, or one per line of a multi-line body with the tag on the first only.
///
/// An empty body still prints its tag: a frame that arrived is news even when it carries nothing
/// worth summarising.
fn say(out: &mut dyn Write, tag: &str, body: &str) -> io::Result<()> {
    let mut lines = body.lines().filter(|l| !l.trim().is_empty()).peekable();
    if lines.peek().is_none() {
        return writeln!(out, "{tag:<TAG_WIDTH$}  ");
    }
    let mut first = true;
    for line in lines {
        if first {
            writeln!(out, "{tag:<TAG_WIDTH$}  {line}")?;
            first = false;
        } else {
            writeln!(out, "{:<TAG_WIDTH$}  {line}", "")?;
        }
    }
    Ok(())
}

/// A tool call's arguments as `key="value"` pairs, truncated.
///
/// Deliberately schema-free: it reads whatever keys the object has rather than the ones a
/// particular tool is known to take, so a new marion verb and a built-in nobody has seen both
/// render without this function being edited. `serde_json` orders object keys, so the line is
/// stable enough to assert on.
fn call_args(input: &Value) -> String {
    let Some(map) = input.as_object() else {
        return match input {
            Value::Null => String::new(),
            other => brief(&other.to_string(), VALUE_CHARS),
        };
    };
    let parts: Vec<String> = map
        .iter()
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k}={:?}", brief(s, VALUE_CHARS)),
            Value::Null | Value::Bool(_) | Value::Number(_) => format!("{k}={v}"),
            Value::Array(a) => format!("{k}=[{} items]", a.len()),
            Value::Object(o) => format!("{k}={{{} keys}}", o.len()),
        })
        .collect();
    brief(&parts.join(" "), LINE_CHARS)
}

/// The text a `tool_result` block carries, whether it is a bare string or the block list 2.1.220
/// also uses.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One `assistant` frame: its content blocks, in order.
fn render_assistant(frame: &Value, out: &mut dyn Write) -> io::Result<()> {
    let blocks = frame["message"]["content"].as_array();
    let empty = blocks.is_none_or(|b| b.is_empty());
    if empty {
        // Rule 1: a turn that carried no blocks is still a turn that happened.
        return say(
            out,
            "root",
            "(an assistant frame carrying no content blocks)",
        );
    }
    for block in blocks.into_iter().flatten() {
        match block["type"].as_str().unwrap_or_default() {
            "text" => say(out, "root", block["text"].as_str().unwrap_or_default())?,
            // Never the thinking itself: it is long, and it is not what a watcher is here for.
            "thinking" => say(
                out,
                "think",
                &format!(
                    "({} characters of reasoning)",
                    block["thinking"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .count()
                ),
            )?,
            "tool_use" => render_tool_use(block, out)?,
            other => say(out, "block", &format!("an assistant `{other}` block"))?,
        }
    }
    Ok(())
}

/// One `tool_use` block. **A marion verb is marked, because it is the interesting event** — a
/// `spawn` is the moment the root stops working alone, and it is what a person running `marion run`
/// is watching for. Everything else is one line naming the tool and summarising its arguments.
fn render_tool_use(block: &Value, out: &mut dyn Write) -> io::Result<()> {
    let name = block["name"]
        .as_str()
        .unwrap_or("<a tool_use block with no name>");
    let args = call_args(&block["input"]);
    match name.strip_prefix("mcp__marion__") {
        Some(verb) => say(out, "MARION", format!("{verb}  {args}").trim_end()),
        None => say(out, "tool", format!("{name}  {args}").trim_end()),
    }
}

/// A returned `TaskContract`, as a sentence — **the one tool result worth reading a schema for.**
///
/// `bridge::spawn_result` answers a `spawn` with the whole contract pretty-printed, so the moment a
/// watcher most cares about — a child came back — would otherwise render as 200 characters of
/// `{ "task_id": …, "requester": …`, which is the JSON dump this renderer exists not to do. The
/// interesting three fields are the child's harness, its status, and the narrative it reported;
/// everything else in a contract is for the journal, which keeps all of it.
///
/// **This reads marion's own types, not a harness's JSON**: the fields are deserialized as
/// [`marion_core::contract::TaskContract`] declares them — the child's `Harness`, the completion's
/// `ExitStatus`, its capped narrative and the branch and commit marion read back — so the status is
/// said with [`ExitStatus::verb_phrase`] like every other place a status is said, and a contract
/// whose shape changed fails to parse and falls back to the generic brief rather than being read
/// wrong. Only these fields, because a watcher's sentence needs no more of it.
fn contract_summary(text: &str) -> Option<String> {
    use marion_core::contract::{Capped, Oid};
    #[derive(serde::Deserialize)]
    struct Contract {
        child: Child,
        completion: Option<Completion>,
    }
    #[derive(serde::Deserialize)]
    struct Child {
        harness: marion_core::harness::Harness,
    }
    #[derive(serde::Deserialize)]
    struct Completion {
        status: ExitStatus,
        narrative: Option<Capped<String>>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        commit: Option<Oid>,
    }
    // A failed spawn puts a one-line account *above* the contract (`bridge::failure_line`), so the
    // JSON starts at the first `{` rather than at byte zero.
    let start = text.find('{')?;
    let contract: Contract = serde_json::from_str(text[start..].trim()).ok()?;
    let completion = contract.completion?;
    let mut summary = format!(
        "the {} child {}",
        contract.child.harness,
        completion.status.verb_phrase()
    );
    if let Some(head) = text[..start]
        .trim()
        .lines()
        .next()
        .filter(|l| !l.is_empty())
    {
        // The failure line marion itself wrote, kept whole: it names what went wrong.
        summary = format!("{}\n{summary}", head.trim());
    }
    let summary = match &completion.narrative {
        Some(n) => format!("{summary}: {}", brief(&n.value, LINE_CHARS)),
        None => format!("{summary}, reporting no narrative"),
    };
    // Where the work is, once its worktree is gone — the one thing a watcher must act on.
    match (&completion.branch, &completion.commit) {
        (Some(branch), Some(commit)) => Some(format!(
            "{summary}\n{}",
            marion_core::contract::landed_line(branch, &commit.0)
        )),
        _ => Some(summary),
    }
}

/// One `user` frame. On this path it is not a person typing: it is the harness feeding tool results
/// back into the conversation, which is how a watcher learns whether a call worked.
fn render_user(frame: &Value, out: &mut dyn Write) -> io::Result<()> {
    let blocks = frame["message"]["content"].as_array();
    if blocks.is_none_or(|b| b.is_empty()) {
        return say(out, "user", "(a user frame carrying no content blocks)");
    }
    for block in blocks.into_iter().flatten() {
        match block["type"].as_str().unwrap_or_default() {
            "tool_result" => {
                let text = result_text(&block["content"]);
                let failed = block["is_error"].as_bool() == Some(true);
                match (contract_summary(&text), failed) {
                    // A returned contract, in a sentence. See [`contract_summary`].
                    (Some(summary), _) => {
                        say(out, if failed { "FAILED" } else { "CHILD" }, &summary)?
                    }
                    // Verbatim: an error is the one thing worth the width.
                    (None, true) => say(out, "FAILED", text.trim())?,
                    (None, false) => say(out, "ok", &brief(text.trim(), LINE_CHARS))?,
                }
            }
            "text" => say(
                out,
                "user",
                &brief(block["text"].as_str().unwrap_or_default(), LINE_CHARS),
            )?,
            other => say(out, "block", &format!("a user `{other}` block"))?,
        }
    }
    Ok(())
}

/// The terminal `result` frame — the run's own verdict, and the last line a watcher sees.
fn render_result(frame: &Value, out: &mut dyn Write) -> io::Result<()> {
    let subtype = frame["subtype"].as_str().unwrap_or("(no subtype)");
    let errored = frame["is_error"].as_bool() == Some(true) || subtype != "success";
    let mut facts = vec![subtype.to_string()];
    if let Some(ms) = frame["duration_ms"].as_u64() {
        facts.push(format!("{:.1}s", ms as f64 / 1000.0));
    }
    if let Some(turns) = frame["num_turns"].as_u64() {
        facts.push(format!("{turns} turns"));
    }
    if let Some(cost) = frame["total_cost_usd"].as_f64().filter(|c| *c > 0.0) {
        facts.push(format!("${cost:.4}"));
    }
    if errored {
        say(out, "FAILED", &facts.join(", "))?;
        // The node's own words about its failure, in full — a truncated error misreports it.
        if let Some(text) = frame["result"].as_str().filter(|t| !t.trim().is_empty()) {
            return say(out, "", text.trim());
        }
        return Ok(());
    }
    say(out, "done", &facts.join(", "))
}

/// Everything `marion run` shows a person, from one [`StreamEvent`].
fn render_event(event: StreamEvent<'_>, out: &mut dyn Write) -> io::Result<()> {
    match event {
        // A line the node wrote that was not JSON at all is almost always a crash or a warning from
        // the harness. Verbatim, and marked as coming from the node rather than from marion.
        StreamEvent::Unparsed(line) => say(out, "stdout", line.trim_end()),
        StreamEvent::Frame(frame) => render_frame(frame, out),
    }
}

/// The live view's state across frames: the one renderer that needs any.
///
/// ACP streams a message as many `agent_message_chunk` updates — often a word each — and a line per
/// chunk would bury the run. So a chunk is held and extended by the next chunk of the same kind,
/// and written by whatever comes next: another frame, a raw line, or the run's end ([`Self::flush`]).
/// Keyed on the frame's shape, so it applies to any stream that has chunks and none that does not.
#[derive(Default)]
struct LiveView {
    pending: Option<(AcpChunk, String)>,
}

impl LiveView {
    fn event(&mut self, event: StreamEvent<'_>, out: &mut dyn Write) -> io::Result<()> {
        if let StreamEvent::Frame(frame) = event
            && frame["method"] == "session/update"
            && let Some((kind, text)) = acp_chunk(&frame["params"]["update"])
        {
            match &mut self.pending {
                Some((held, so_far)) if *held == kind => so_far.push_str(text),
                _ => {
                    self.flush(out)?;
                    self.pending = Some((kind, text.to_string()));
                }
            }
            return Ok(());
        }
        self.flush(out)?;
        render_event(event, out)
    }

    fn flush(&mut self, out: &mut dyn Write) -> io::Result<()> {
        match self.pending.take() {
            Some((kind, text)) => render_acp_chunk(kind, &text, out),
            None => Ok(()),
        }
    }
}

/// The two kinds of ACP text chunk, kept apart so a thought never runs into a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpChunk {
    Message,
    Thought,
}

/// The kind and text of an ACP `session/update` body that is a text chunk, else `None`.
fn acp_chunk(update: &Value) -> Option<(AcpChunk, &str)> {
    let kind = match update["sessionUpdate"].as_str()? {
        "agent_message_chunk" => AcpChunk::Message,
        "agent_thought_chunk" => AcpChunk::Thought,
        _ => return None,
    };
    Some((kind, update["content"]["text"].as_str().unwrap_or_default()))
}

/// A run of chunk text. A thought is summarised, never shown, as `render_assistant` treats
/// `thinking`: it is long, and it is not what a watcher is here for.
fn render_acp_chunk(kind: AcpChunk, text: &str, out: &mut dyn Write) -> io::Result<()> {
    match kind {
        AcpChunk::Message => say(out, "root", text),
        AcpChunk::Thought => say(
            out,
            "think",
            &format!("({} characters of reasoning)", text.chars().count()),
        ),
    }
}

/// A JSON-RPC 2.0 frame — ACP's shape, whichever agent wrote it.
fn render_json_rpc(frame: &Value, out: &mut dyn Write) -> io::Result<()> {
    if frame["method"] == "session/update" {
        return render_acp_update(&frame["params"]["update"], out);
    }
    // A request or notification the agent sent marion: the driver answers it; this names it.
    if let Some(method) = frame["method"].as_str() {
        return say(out, "acp", method);
    }
    if let Some(error) = frame.get("error") {
        return say(
            out,
            "FAILED",
            error["message"]
                .as_str()
                .unwrap_or("an error with no message"),
        );
    }
    let result = &frame["result"];
    if let Some(reason) = result["stopReason"].as_str() {
        // `refusal` is the one ending that is a failure (`acp::parse_stream` reads it the same way).
        return say(
            out,
            if reason == "refusal" {
                "FAILED"
            } else {
                "done"
            },
            reason,
        );
    }
    if let Some(name) = result["agentInfo"]["name"].as_str() {
        let version = result["agentInfo"]["version"].as_str().unwrap_or("?");
        let wire = result["protocolVersion"].as_u64().unwrap_or_default();
        return say(out, "session", &format!("{name} {version}, ACP v{wire}"));
    }
    if let Some(session) = result["sessionId"].as_str() {
        return say(out, "session", &format!("session {session}"));
    }
    say(
        out,
        "acp",
        &format!("the answer to request {}", frame["id"]),
    )
}

/// One ACP `session/update`, by its `sessionUpdate` kind.
fn render_acp_update(update: &Value, out: &mut dyn Write) -> io::Result<()> {
    if let Some((chunk, text)) = acp_chunk(update) {
        return render_acp_chunk(chunk, text, out);
    }
    match update["sessionUpdate"].as_str().unwrap_or_default() {
        "tool_call" => {
            let title = update["title"]
                .as_str()
                .unwrap_or("<a tool call with no title>");
            say(
                out,
                "tool",
                format!("{title}  {}", call_args(&update["rawInput"])).trim_end(),
            )
        }
        "tool_call_update" => render_acp_tool_update(update, out),
        "" => say(out, "acp", "a `session/update` with no `sessionUpdate`"),
        other => say(out, "acp", &format!("a `{other}` update")),
    }
}

/// A `tool_call_update`, by its status. A finished `spawn` answers with a whole contract, which
/// reads as the same sentence the stream-json view gives it ([`contract_summary`]).
fn render_acp_tool_update(update: &Value, out: &mut dyn Write) -> io::Result<()> {
    let status = update["status"].as_str().unwrap_or("(no status)");
    let text: String = update["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["content"]["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    // Some agents mirror the MCP result whole (`{"content":[{"text":…}]}`); the contract is inside.
    let text = mcp_result_text(&text).unwrap_or(text);
    let failed = status == "failed";
    match (contract_summary(&text), status) {
        (Some(summary), _) => say(out, if failed { "FAILED" } else { "CHILD" }, &summary),
        (None, "failed") => say(out, "FAILED", text.trim()),
        (None, "completed") => say(out, "ok", &brief(text.trim(), LINE_CHARS)),
        (None, other) => say(
            out,
            "tool",
            &format!("{} {other}", update["toolCallId"].as_str().unwrap_or("?")),
        ),
    }
}

/// The text blocks of an MCP `tools/call` result carried as a string, or `None` if it is not one.
fn mcp_result_text(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    let blocks: Vec<&str> = v["content"]
        .as_array()?
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

/// One parsed frame, by its `type`; each kind has a renderer of its own. A JSON-RPC frame is
/// ACP's and has no `type`, so its shape is read first. A frame of a JSON-RPC server with no
/// `jsonrpc` member (codex's app-server, S36 P2) is named by its `method`.
fn render_frame(frame: &Value, out: &mut dyn Write) -> io::Result<()> {
    if frame["jsonrpc"] == "2.0" {
        return render_json_rpc(frame, out);
    }
    let subtype = frame["subtype"].as_str().unwrap_or_default();
    let kind = frame["type"].as_str().or_else(|| frame["method"].as_str());
    match kind.unwrap_or_default() {
        "assistant" => render_assistant(frame, out),
        "user" => render_user(frame, out),
        "result" => render_result(frame, out),
        "system" if subtype == "init" => render_session_init(frame, out),
        "control_request" => render_control_request(frame, out),
        // The item-stream shape (codex `exec --json`): a finished message or reasoning item is
        // the root's words, like an `assistant` text or `thinking` block. Other items keep the
        // generic one-line name below.
        "item.completed" if frame["item"]["type"] == "agent_message" => say(
            out,
            "root",
            frame["item"]["text"].as_str().unwrap_or_default(),
        ),
        "item.completed" if frame["item"]["type"] == "reasoning" => say(
            out,
            "think",
            &format!(
                "({} characters of reasoning)",
                frame["item"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .count()
            ),
        ),
        // Never its body: the `initialize` reply alone is ~30 kB of session catalogue.
        "control_response" => say(
            out,
            "control",
            &format!(
                "{} to {}",
                frame["response"]["subtype"]
                    .as_str()
                    .unwrap_or("(no subtype)"),
                frame["response"]["request_id"]
                    .as_str()
                    .unwrap_or("(no request_id)")
            ),
        ),
        kind => render_unknown_frame(kind, subtype, out),
    }
}

/// The `system/init` frame as one `session` line: model, tool count and each MCP server's status.
fn render_session_init(frame: &Value, out: &mut dyn Write) -> io::Result<()> {
    let model = frame["model"].as_str().unwrap_or("(unnamed)");
    let tools = frame["tools"].as_array().map_or(0, Vec::len);
    let mcp: Vec<String> = frame["mcp_servers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            format!(
                "{}={}",
                s["name"].as_str().unwrap_or("?"),
                s["status"].as_str().unwrap_or("?")
            )
        })
        .collect();
    say(
        out,
        "session",
        &format!(
            "model {model}, {tools} tools, mcp: {}",
            if mcp.is_empty() {
                "none".to_string()
            } else {
                mcp.join(" ")
            }
        ),
    )
}

/// A `control_request`: a permission ask is `PERMIT` and says what marion will do about it; any
/// other request is `control` and says marion is refusing it.
fn render_control_request(frame: &Value, out: &mut dyn Write) -> io::Result<()> {
    if frame["request"]["subtype"] == "can_use_tool" {
        return say(
            out,
            "PERMIT",
            &format!(
                "{} — nobody is attached to answer it; it will be denied when its wait runs out",
                frame["request"]["tool_name"]
                    .as_str()
                    .unwrap_or("(unnamed tool)")
            ),
        );
    }
    say(
        out,
        "control",
        &format!(
            "{} — marion does not implement it and is answering with an error",
            frame["request"]["subtype"]
                .as_str()
                .unwrap_or("(no subtype)")
        ),
    )
}

/// Rule 1. Not a dump and not silence: the kind, and its subtype when it has one.
fn render_unknown_frame(kind: &str, subtype: &str, out: &mut dyn Write) -> io::Result<()> {
    let body = match (kind, subtype) {
        ("", _) => "a stdout frame with no `type` or `method` field".to_string(),
        (other, "") => other.to_string(),
        (other, s) => format!("{other}/{s}"),
    };
    say(out, "frame", &body)
}

/// One [`ChildEvent`], as the line a watcher sees.
///
/// **These are the minutes that used to be blank.** The root's own frames stop at
/// `MARION spawn …`: the child is driven inside the bridge's process, whose stdout is an MCP stream
/// and which must stay silent, so nothing about it could ever reach here directly. What reaches
/// here instead is what marion already wrote down — the journal — read back by
/// [`marion_supervisor::watch`].
fn render_child(event: &ChildEvent, out: &mut dyn Write) -> io::Result<()> {
    match event {
        ChildEvent::Started {
            agent_type,
            harness,
            depth,
            pid,
            ..
        } => render_child_started(agent_type, harness.as_ref(), *depth, *pid, out),
        ChildEvent::Aborted {
            agent_type, reason, ..
        } => say(
            out,
            "FAILED",
            &format!("{} never started: {reason}", child_name(agent_type)),
        ),
        ChildEvent::Exited {
            agent_type,
            status,
            exit,
            ..
        } => render_child_exited(agent_type, *status, exit.as_ref(), out),
        ChildEvent::Denied {
            agent_type,
            tool,
            reason,
            ..
        } => say(
            out,
            "PERMIT",
            &format!("{} was denied {tool}: {reason}", child_name(agent_type)),
        ),
        // The viewer stopping is news in its own right — the alternative is a view that quietly
        // stops updating, which reads exactly like a run in which nothing further happened.
        ChildEvent::Stopped { reason } => say(out, "view", reason),
    }
}

/// What a child is called in its line.
///
/// A child with no intent record is a real case, not a defect: its intent may predate this
/// watch's cursor. Naming it "a child" is honest; inventing an agent type would not be.
fn child_name(agent_type: &Option<String>) -> String {
    agent_type.clone().unwrap_or_else(|| "a child".into())
}

/// The `CHILD … started` line: the facts the intent record had, and only those.
fn render_child_started(
    agent_type: &Option<String>,
    harness: Option<&marion_core::harness::Harness>,
    depth: Option<u32>,
    pid: Option<i32>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut facts = Vec::new();
    if let Some(h) = harness {
        facts.push(h.to_string());
    }
    if let Some(d) = depth {
        facts.push(format!("depth {d}"));
    }
    if let Some(p) = pid {
        facts.push(format!("pid {p}"));
    }
    say(
        out,
        "CHILD",
        &format!("{} started ({})", child_name(agent_type), facts.join(", ")),
    )
}

/// The `ended` line: `CHILD` for a success, `FAILED` otherwise, with [`exit_detail`]'s text.
fn render_child_exited(
    agent_type: &Option<String>,
    status: Option<ExitStatus>,
    exit: Option<&marion_core::contract::ProcessExit>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let verdict = status.map_or("an unrecorded status", ExitStatus::word);
    let ok = status == Some(ExitStatus::Ok);
    let detail = exit_detail(exit, ok);
    say(
        out,
        if ok { "CHILD" } else { "FAILED" },
        &format!("{} ended: {verdict}{detail}", child_name(agent_type)),
    )
}

/// The exit record's description as the tail of the `exited` line, or nothing when it has none.
fn exit_detail(exit: Option<&marion_core::contract::ProcessExit>, ok: bool) -> String {
    match exit {
        Some(e) if !e.description.trim().is_empty() => {
            // The same rule the rest of this renderer follows, and it earns its keep here:
            // §6.7's description carries the child's own stderr, so a *successful* child
            // that merely warned would otherwise drag its warnings across the terminal. A
            // failure keeps every byte — that is the text that explains it.
            let d = e.description.trim();
            match ok {
                true => format!(" — {}", brief(d, LINE_CHARS)),
                false => format!(" — {d}"),
            }
        }
        _ => String::new(),
    }
}

/// **Two writers, one terminal.**
///
/// The root's frames are rendered on the thread driving the run; the journal's child events are
/// rendered on the polling thread. Both write *multi-line* renders — an assistant turn is as many
/// lines as it has prose — and two threads writing to one fd will interleave between those lines
/// unless something stops them. The result is a `CHILD` line spliced into the middle of a
/// paragraph, which reads as a bug in marion rather than a bug in a renderer, and which shows up
/// only under load.
///
/// **The lock is the writer**, rather than a `Mutex<()>` next to one: a bare flag guarding an
/// implicit resource is the shape that gets written around six months later, because nothing about
/// `io::stderr()` says it was supposed to be taken. Here there is no way to reach the writer
/// without holding it, and it is held across a **whole render** rather than around each `write`
/// call — the unit that must not be split is the event, not the byte.
///
/// A poisoned lock is taken anyway. A viewer must not be the thing that ends a run.
struct Terminal<W: Write> {
    out: std::sync::Mutex<W>,
}

impl<W: Write> Terminal<W> {
    fn new(out: W) -> Self {
        Self {
            out: std::sync::Mutex::new(out),
        }
    }

    /// Render one event, whole, with nobody else able to write in the middle of it.
    ///
    /// Not held one moment longer: `io::Stderr` flushes per line, so a line reaches the terminal as
    /// its event is read, which is the entire point of streaming it.
    fn show(&self, render: &dyn Fn(&mut dyn Write)) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        render(&mut *out);
    }
}

/// Follow the journal until the run is over, **and once more after that.**
///
/// The order of the loop is the whole content of this function: **the return is decided after a
/// poll, never before one.** A child that exits in the last moments before the root finishes — the
/// bridge writes `Exited` and `ContractPersisted` just before returning the `spawn` result, and the
/// root may finish inside one poll interval of that — is therefore still announced. A loop that
/// returned on the flag *before* polling would drop exactly those records, and the view would end
/// on a lie: the last thing a person saw would be a child that started and never finished.
///
/// Whether the flag is read before or after the poll within an iteration does not matter, and the
/// mutation check confirmed it: both orders still poll before returning. Only hoisting the return
/// above the poll breaks it, which is the version the test kills.
///
/// `stopped` and `nap` are parameters so the loop can be tested without a clock or a run.
fn follow_journal(
    watch: &mut JournalWatch,
    stopped: &dyn Fn() -> bool,
    nap: &dyn Fn(),
    emit: &mut dyn FnMut(&ChildEvent),
) {
    loop {
        let last = stopped();
        for event in watch.poll() {
            emit(&event);
        }
        if last {
            return;
        }
        nap();
    }
}

/// The journal tail's **safety** poll while a run is in flight.
///
/// Not the latency: the tail waits on the journal's change notification and on the run's stop
/// flag ([`nap_until_the_journal_changes`]), so a record is read as soon as it is written and a
/// finished run stops at once. This bounds only what a watch could miss. It used to be the
/// latency, at 100 ms — ten `open`+`metadata` pairs and ten wakeups a second for the whole run.
const JOURNAL_POLL: StdDuration = StdDuration::from_secs(1);

/// [`follow_journal`]'s pause: until the journal is written, the run stops, or `safety` passes
/// ([`JOURNAL_POLL`] in production).
///
/// **Wait, then re-arm**, and the poll comes after: a write that lands between the poll and this
/// wait left an event pending since the last re-arm, so the wait returns at once, and a write that
/// lands after the re-arm is read by the poll that follows. Nothing falls between.
fn nap_until_the_journal_changes(
    file: &std::cell::RefCell<marion_supervisor::wake::Watch>,
    stop: &marion_supervisor::wake::Flag,
    safety: StdDuration,
) {
    let mut file = file.borrow_mut();
    {
        let fds: Vec<_> = file.fd().into_iter().chain(stop.fd()).collect();
        if fds.is_empty() {
            std::thread::sleep(safety);
        } else {
            marion_supervisor::wake::wait_readable(&fds, Some(safety));
        }
    }
    file.rearm();
}

/// `marion run`'s half of §7.3's voluntary quit: **a run that ends says so.**
///
/// §7.3.1 is absolute — *"a crashed, SIGKILLed, or otherwise vanished client MUST leave every node
/// exactly as it was"*, and the supervisor sees an identical close either way, so *"the only
/// evidence of intent that can ever exist is a `session/quit` that arrived first"*. A `marion run`
/// that simply dropped its socket would therefore be indistinguishable from one that was killed,
/// and the supervisor would — correctly, per §7.3.1 — treat a finished run as a crash and stay
/// resident over a journal in which nothing is left to supervise.
///
/// So this guard sends §7.3.2's disposition **(b), `DetachAll`**, and reports everything the
/// supervisor answers with.
///
/// **§7.3.2's "never silently detach" is the whole reason the answer is printed.** *"They MUST tell
/// the operator both how to re-attach and how to stop the fleet without one. Detaching into silence
/// is worse than killing, because the operator does not know they now own something."* A guard that
/// printed only `Resident(NonTerminalNode)` satisfied the letter of *"say something"* and none of
/// that rule: the nodes still running, the ones that can hit a permission gate unattended (§7.3.2's
/// stated cost of (b), §11 item 22), the socket to dial and the call that stops the fleet were all
/// in the response and none of them reached the operator. All of it is printed now, and the
/// resident and exiting cases print the same detach facts because a fleet is equally detached
/// either way.
///
/// **Where this guard does *not* run, stated rather than implied.** `Drop` covers an ordinary
/// return from `main` and an unwind. It does **not** run on `std::process::exit`, on a `panic =
/// "abort"` build, or on any signal — SIGINT from the operator's own Ctrl-C, SIGTERM, SIGKILL. On
/// every one of those the supervisor sees exactly what it sees when a TUI dies, and §7.3.1 gives
/// the right answer for that: nothing happens to any node, and the supervisor waits out §5.7's
/// full grace rather than treating the close as a decision. That is a worse outcome than this
/// guard's, not an unsafe one, and *"every exit path"* would be a false claim — the honest one is
/// that marion installs no signal handler and §11 item 28 records what closing that would take.
///
/// **What it must not do is turn a failing run into a failing process.** It runs during unwind, so
/// every step is fallible-and-ignored and nothing here may panic: a `println!`/`eprintln!` panics
/// when stderr is closed, and a panic during an unwind aborts the process — a `marion run` whose
/// real error would then never be printed at all. Every write goes through `writeln!` with its
/// result dropped for that reason, and both waits are bounded by named constants.
struct SupervisorSession {
    stream: std::os::unix::net::UnixStream,
    /// **The one reader on this connection**, held here rather than made per call.
    ///
    /// It has to be one, and that is not tidiness: a `BufReader` reads ahead, so a second one
    /// constructed for the quit would start behind bytes the first had already buffered. It became
    /// load-bearing with step 6 — until then this connection carried one request and one response
    /// and nothing else, and now it carries the root's whole event stream.
    lines: io::BufReader<std::os::unix::net::UnixStream>,
    socket: PathBuf,
    next_id: i64,
}

/// What one leg of the run wants off the connection.
enum Awaited {
    /// The answer to request `id`, with every notification that arrived first handed to `on_note`.
    Response(i64),
}

impl SupervisorSession {
    fn send(&mut self, call: marion_core::proto::Call) -> Result<i64, String> {
        use std::io::Write as _;
        let id = self.next_id;
        self.next_id += 1;
        let frame = marion_core::proto::Frame::Request(marion_core::proto::Request::new(
            marion_core::proto::RequestId::Number(id),
            call,
        ));
        self.stream
            .write_all(frame.to_line().as_bytes())
            .and_then(|()| self.stream.flush())
            .map_err(|e| supervisor_unreachable(&self.socket, &e.to_string()))?;
        Ok(id)
    }

    /// Read one frame, or say why there will not be another.
    ///
    /// **EOF is an error here and not an end.** A supervisor that closed mid-run took the root's
    /// only live channel with it, and a client that returned success on a closed socket would be
    /// reporting a run it stopped watching.
    fn next_frame(&mut self) -> Result<marion_core::proto::Frame, String> {
        use std::io::BufRead as _;
        let mut line = String::new();
        match self.lines.read_line(&mut line) {
            Ok(0) => Err(supervisor_unreachable(
                &self.socket,
                "it closed the connection while the run was still in flight",
            )),
            Ok(_) => marion_core::proto::Frame::from_line(&line).map_err(|e| {
                supervisor_unreachable(
                    &self.socket,
                    &format!("it sent a frame marion cannot read: {e}"),
                )
            }),
            Err(e) => Err(supervisor_unreachable(&self.socket, &e.to_string())),
        }
    }

    /// Pump frames, handing every notification to `on_note`, until the awaited thing arrives.
    fn pump(
        &mut self,
        want: Awaited,
        on_note: &mut dyn FnMut(marion_core::proto::notify::Event),
    ) -> Result<marion_core::proto::Outcome, String> {
        loop {
            match self.next_frame()? {
                marion_core::proto::Frame::Notification(n) => on_note(n.event),
                marion_core::proto::Frame::Response(r) => {
                    let Awaited::Response(id) = want;
                    if r.id == marion_core::proto::RequestId::Number(id) {
                        return Ok(r.outcome);
                    }
                    // A response marion is not waiting for cannot happen — one request is in
                    // flight at a time on this connection — and going quiet about it would hide a
                    // correlation bug behind a hang.
                    return Err(format!(
                        "marion: this project's supervisor answered a request marion did not send \
                         ({:?}); the connection is no longer trustworthy",
                        r.id
                    ));
                }
                other => {
                    return Err(format!(
                        "marion: this project's supervisor sent an unexpected frame: {other:?}"
                    ));
                }
            }
        }
    }
}

/// **The refusal a run dies with when its supervisor is not there.**
///
/// One sentence, in marion's own voice, naming what failed — and no fallback. Until step 6 a
/// missing supervisor cost this run the socket and not the work, because the root was driven in
/// this process; it is not any more, so there is nothing to fall back *to*, and inventing one would
/// be the silent-degradation shape this codebase keeps deleting: a run that looked identical while
/// leaving a live root nothing could reach, kill or re-attach to.
fn supervisor_unreachable(socket: &Path, why: &str) -> String {
    format!(
        "marion: this project's supervisor is what runs the root, and marion could not use it: \
         {why} ({}). Nothing was started. There is no in-process fallback: a root marion drove \
         itself would be one no supervisor could watch, stop or hand to `marion attach`.",
        socket.display()
    )
}

/// How long [`SupervisorSession::drop`] will wait for a supervisor that answered `Exiting`.
///
/// **A bound, not a measurement**, and it decides nothing: a supervisor that is going has already
/// journaled its exit record before the accept loop breaks, so what is being waited for is one
/// `unlink`. Nothing asserts on how long it takes.
const SUPERVISOR_EXIT_WAIT: StdDuration = StdDuration::from_secs(5);

/// The §5.7 idle grace `marion run` asks a supervisor it starts to use.
///
/// **The supervisor's own default, deliberately not a number this client chose.** See the call
/// site: whichever client starts a supervisor fixes its grace for every later client, so a value
/// justified by *this* command's lifetime would be imposed on a TUI that had no say in it.
const RUN_IDLE_GRACE: StdDuration = marion_supervisor::serve::DEFAULT_IDLE_GRACE;

/// How long [`SupervisorSession::drop`] will wait for the answer to its own quit.
///
/// **Named so it cannot be omitted.** `set_read_timeout` is fallible, and a guard that ignored the
/// failure and read anyway would block for as long as a wedged supervisor cared to hold the
/// socket — during an unwind, with the run's real error still unprinted. A timeout that cannot be
/// set is therefore a reason to stop, not a reason to read without one.
const SUPERVISOR_REPLY_WAIT: StdDuration = StdDuration::from_secs(10);

/// How many frames [`SupervisorSession::drop`] will step over looking for its own answer.
///
/// A count and not a time, because what it is stepping over is a queue that was already written:
/// the run is finished by the time the guard sends its quit, so anything still arriving is the tail
/// of the root's stream and is finite. Generous enough that a chatty tail cannot swallow the
/// report, small enough that it is not a loop.
const QUIT_REPLY_FRAMES: usize = 4096;

/// §7.3.2's disclosure, rendered from the supervisor's own answer and nothing else.
///
/// Five facts, because §7.3.2 names five and a detach that reports fewer is the *"detaching into
/// silence"* the rule forbids: what the supervisor is doing and why, **which nodes are still
/// running**, which of them can burn their permission bound unattended (§11 item 22 — the cost the
/// section says MUST be stated at the point of choosing (b), not discovered afterwards), how to get
/// back, and how to stop the fleet without getting back.
///
/// A free function so the sentence can be tested without a socket, a supervisor or an unwind. It
/// hands back the disposition it rendered, because the caller's only remaining decision — whether
/// to wait for the socket to go — is the same fact and must not be re-derived from a second match.
fn detach_report(
    outcome: &marion_core::proto::QuitOutcome,
) -> Option<(marion_core::proto::SupervisorDisposition, String)> {
    use std::fmt::Write as _;
    // The wire's `guidance` names the protocol calls a client makes; a person is told marion's
    // own commands instead, so it is not read here.
    let (supervisor, detached, gate_exposed, reaped) = match outcome {
        marion_core::proto::QuitOutcome::Detached {
            detached,
            gate_exposed,
            supervisor,
            ..
        } => (supervisor, detached, gate_exposed, Vec::new()),
        marion_core::proto::QuitOutcome::ReapedAndDetached {
            reaped,
            detached,
            gate_exposed,
            supervisor,
            ..
        } => (
            supervisor,
            detached,
            gate_exposed,
            reaped.iter().map(|n| n.0.clone()).collect(),
        ),
        // A `Killed` outcome cannot arrive here: this guard only ever sends `DetachAll`.
        marion_core::proto::QuitOutcome::Killed { .. } => return None,
    };
    let names = |ids: &[marion_core::contract::AgentId]| {
        ids.iter()
            .map(|i| i.0.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = String::new();
    match supervisor {
        marion_core::proto::SupervisorDisposition::Resident(reason) => {
            let why = match reason {
                marion_core::proto::ResidentReason::NonTerminalNode => "an agent is still running",
                marion_core::proto::ResidentReason::BlockedNode => "an agent is waiting",
                marion_core::proto::ResidentReason::SpawnOutstanding => {
                    "an agent is still starting"
                }
                marion_core::proto::ResidentReason::UnconfirmedReapIntent => {
                    "it has not finished tidying up an agent"
                }
                marion_core::proto::ResidentReason::RegistryStopped => {
                    "it could not read the rest of its journal, so it keeps what it knows"
                }
            };
            let _ = writeln!(
                out,
                "marion: this project's supervisor keeps running ({why}); the next marion will \
                 find it rather than start one"
            );
        }
        marion_core::proto::SupervisorDisposition::Exiting => {
            let _ = writeln!(
                out,
                "marion: nothing holds this project's supervisor any more, so it is leaving and \
                 journaling that it did"
            );
        }
    }
    if !reaped.is_empty() {
        let _ = writeln!(
            out,
            "marion: reaped and resumable: {} (transcript intact; `marion resume <id>` continues it)",
            reaped.join(", ")
        );
    }
    if detached.is_empty() {
        let _ = writeln!(out, "marion: no node was left running.");
    } else {
        // One line for the fleet — what is running, and how to watch, open and stop it with
        // marion's own commands — and a second only for the cost §7.3.2 says must be stated: nodes
        // that will be refused any permission they ask for, since nobody can approve it.
        let _ = writeln!(
            out,
            "marion: still running in the background: {}; `marion ls` to watch, `marion attach \
             {}` to open a pane agent, `marion cancel <id>` to stop one",
            names(detached),
            detached[0].0,
        );
        if !gate_exposed.is_empty() {
            let _ = writeln!(
                out,
                "marion: nobody can approve a permission for {}, so any they ask for is refused",
                names(gate_exposed)
            );
        }
    }
    Some((*supervisor, out))
}

impl Drop for SupervisorSession {
    fn drop(&mut self) {
        // Nothing below may panic: this can run during an unwind, and a panic there aborts the
        // process before the run's own error is printed. `writeln!` to a locked stderr rather than
        // `eprintln!`, which panics on a write failure, and every result is deliberately dropped.
        let mut err = io::stderr();
        if !self.request_detach_all() {
            return;
        }
        let Some(response) = self.quit_response() else {
            return;
        };
        let marion_core::proto::Outcome::Result(body) = response.outcome else {
            return;
        };
        let Ok(marion_core::proto::MethodResult::SessionQuit(result)) =
            marion_core::proto::Method::SessionQuit.decode_result(&body)
        else {
            return;
        };
        let Some((supervisor, report)) = detach_report(&result.outcome) else {
            return;
        };
        let _ = write!(err, "{report}");
        if supervisor == marion_core::proto::SupervisorDisposition::Exiting {
            self.wait_for_supervisor_exit(&mut err);
        }
    }
}

impl SupervisorSession {
    /// Send `session/quit DetachAll` and bound the wait for its answer. `false` when either step
    /// failed, and then there is nothing more this session can do or say.
    fn request_detach_all(&mut self) -> bool {
        let frame = marion_core::proto::Frame::Request(marion_core::proto::Request::new(
            marion_core::proto::RequestId::Number(1),
            marion_core::proto::Call::SessionQuit(marion_core::proto::params::SessionQuitParams {
                disposition: marion_core::proto::QuitDisposition::DetachAll,
            }),
        ));
        if self
            .stream
            .write_all(frame.to_line().as_bytes())
            .and_then(|()| self.stream.flush())
            .is_err()
        {
            // The supervisor is already gone or unreachable. Nothing to report and nothing to do:
            // §7.3.1's invariant means the nodes are untouched either way.
            return false;
        }
        // See [`SUPERVISOR_REPLY_WAIT`]: reading without a bound is the one option that is
        // worse than not reading at all.
        self.stream
            .set_read_timeout(Some(SUPERVISOR_REPLY_WAIT))
            .is_ok()
    }

    /// The quit's response, skipping the notifications that may arrive ahead of it.
    ///
    /// **Skipping notifications rather than reading one line.** This connection is subscribed to
    /// the root's stream, so the next line is quite as likely to be a `node/event` as the
    /// answer — and a guard that read exactly one line would report nothing on every run whose
    /// node said one more thing on the way out. Bounded twice over: by the read timeout
    /// [`request_detach_all`](Self::request_detach_all) set, which applies per read, and by the
    /// count, so a supervisor that streams for ever cannot hold an unwinding process. `None` on
    /// EOF, a read failure, an unreadable line or the count running out.
    fn quit_response(&mut self) -> Option<marion_core::proto::Response> {
        for _ in 0..QUIT_REPLY_FRAMES {
            let mut line = String::new();
            match self.lines.read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            match marion_core::proto::Frame::from_line(&line) {
                Ok(marion_core::proto::Frame::Response(r)) => return Some(r),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// Wait up to [`SUPERVISOR_EXIT_WAIT`] for an exiting supervisor's socket to go, and say so
    /// on `err` if it is still there.
    fn wait_for_supervisor_exit(&self, err: &mut io::Stderr) {
        let gone = marion_supervisor::wake::poll_until(
            SUPERVISOR_EXIT_WAIT,
            StdDuration::from_millis(2),
            || !self.socket.exists(),
        );
        if !gone {
            let _ = writeln!(
                err,
                "marion: this project's supervisor said it was exiting and {} is still there \
                 after {} s; it may still be shutting down",
                self.socket.display(),
                SUPERVISOR_EXIT_WAIT.as_secs()
            );
        }
    }
}

fn main() -> ExitCode {
    let native_facades = production_native_facades();
    dispatch_native_facade_or_legacy(
        std::env::args_os().skip(1),
        &native_facades,
        // §5.7's start, the same one `run_main` and `resume_main` take: dial this project's
        // supervisor and start one only if nothing answers. A facade has no marion flags, so what
        // stage 3 is told is the zero-configuration pair — the operator's own login and no
        // endpoint — and the grace every other client leaves to the supervisor.
        || {
            marion_supervisor::facade_cli::ensure_supervisor_then_connect(|state, project_root| {
                detach::Launch {
                    program: supervisor_binary(),
                    state_dir: state.to_path_buf(),
                    project_root: project_root.to_path_buf(),
                    idle_grace: RUN_IDLE_GRACE,
                    auth: marion_harness::Auth::Inherited,
                    base_url: None,
                }
            })
        },
        marion_supervisor::facade_cli::relay_native_facade,
        io::stderr(),
        legacy_main,
    )
}

/// The `marion-supervisor` beside this binary, or its bare name for `$PATH` when there is none —
/// what `marion run`, `marion resume` and the facade all start.
fn supervisor_binary() -> PathBuf {
    match std::env::current_exe().map(|p| p.with_file_name("marion-supervisor")) {
        Ok(p) if p.exists() => p,
        _ => PathBuf::from("marion-supervisor"),
    }
}

fn legacy_main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(first) = argv.first().map(String::as_str) else {
        // Bare `marion`: the home screen, on a terminal. Anywhere else there is nobody to press a
        // key, so it prints the help and fails rather than drawing into a pipe.
        return home_main();
    };
    match first {
        "--help" | "-h" | "help" => match argv.get(1).map(|w| (w, cli::verb(w))) {
            None => println!("{}", cli::top_help()),
            Some((_, Some(verb))) => println!("{}", (verb.help)()),
            Some((word, None)) => return unknown_command(word),
        },
        "--version" | "-V" => println!("marion {}", env!("CARGO_PKG_VERSION")),
        word => match cli::verb(word) {
            Some(verb) => return cli::dispatch(verb, &argv),
            // `marion <anything> --help` still answers with the help, as it always has.
            None if cli::asks_for_help(&argv[1..]) => println!("{}", cli::top_help()),
            None => return unknown_command(word),
        },
    }
    ExitCode::SUCCESS
}

/// A first word that is no command: one line naming it, not a screen of help that would bury what
/// went wrong.
fn unknown_command(word: &str) -> ExitCode {
    eprintln!(
        "marion: unknown command `{word}`; `marion run <agent-type> --prompt <text>` runs an \
         agent, and `marion --help` lists every command"
    );
    ExitCode::from(2)
}

/// `marion login` and `marion logout`, parsed by their own module.
fn login_main(argv: &[String]) -> Result<ExitCode, Exit> {
    if cli::asks_for_help(&argv[1..]) {
        return Err(Exit::Help);
    }
    Ok(marion_supervisor::login::main(argv))
}

/// `marion profile`, parsed by its own module.
fn profile_main(argv: &[String]) -> Result<ExitCode, Exit> {
    if cli::asks_for_help(&argv[1..]) {
        return Err(Exit::Help);
    }
    Ok(marion_supervisor::profile_cli::main(&argv[1..]))
}

/// `marion trust`, parsed by its own module.
fn trust_main(argv: &[String]) -> Result<ExitCode, Exit> {
    if cli::asks_for_help(&argv[1..]) {
        return Err(Exit::Help);
    }
    Ok(marion_supervisor::trust::cli_main(&argv[1..]))
}

/// `marion doctor …` is `marion-supervisor doctor …`: the supervisor owns the probes, and this
/// replaces the process with it so its output, exit code and signals are the doctor's own.
fn doctor_main(argv: &[String]) -> Result<ExitCode, Exit> {
    use std::os::unix::process::CommandExt;
    if cli::asks_for_help(&argv[1..]) {
        return Err(Exit::Help);
    }
    let supervisor = supervisor_binary();
    let mut doctor = std::process::Command::new(&supervisor);
    doctor.arg("doctor").args(&argv[1..]);
    // So the doctor reports this marion, not whichever one it would find on its own.
    if let Ok(me) = std::env::current_exe() {
        doctor.env(marion_supervisor::preflight::CLIENT_EXE_ENV, me);
    }
    let e = doctor.exec();
    eprintln!(
        "marion: cannot run {} doctor: {e}. marion-supervisor is installed with marion and must \
         sit in the same directory",
        supervisor.display()
    );
    Ok(ExitCode::FAILURE)
}

/// `marion run <agent-type> --prompt <text> …`: start a root over the socket and render it until it
/// ends, or return once it has started under `--pane` or `--detach`.
///
/// [`run`] answers with a `Result` so that every stage's refusal is one `?`. A refusal has already
/// printed its sentence and carries only the exit code, so `Err` and `Ok` are both an exit code and
/// folding them here loses nothing.
fn run_main(argv: &[String]) -> Result<ExitCode, Exit> {
    let args = parse_args(argv)?;
    Ok(match run(&args) {
        Ok(code) | Err(code) => code,
    })
}

/// What a run resolves before it can name a supervisor: the agent type, the repository, the state
/// directory and the vendor endpoint.
struct RunTarget {
    agent_type: marion_core::agent_type::AgentType,
    repo: PathBuf,
    state: PathBuf,
    base_url: Option<String>,
}

/// Resolve [`RunTarget`] from the arguments and the environment. Each refusal prints its own
/// sentence and answers the exit code.
fn resolve_run_target(args: &Args) -> Result<RunTarget, ExitCode> {
    let (repo, state) = resolve_project(&args.place).ok_or(ExitCode::FAILURE)?;
    // After the repository, because the table is the repository's: the built-ins plus its
    // `.marion/agents.toml`. `root::prepare` resolves the same table again one layer down.
    let types = match marion_supervisor::run::agent_types(&repo) {
        Ok(types) => types,
        Err(e) => {
            eprintln!("marion: {e}");
            return Err(ExitCode::FAILURE);
        }
    };
    let Some(agent_type) = types.resolve(&args.agent_type) else {
        eprintln!(
            "marion: unknown agent type {:?}; known: {}",
            args.agent_type,
            types.names().join(", ")
        );
        return Err(ExitCode::FAILURE);
    };
    let base_url = endpoint(&args.backend)?;
    Ok(RunTarget {
        agent_type,
        repo,
        state,
        base_url,
    })
}

fn run(args: &Args) -> Result<ExitCode, ExitCode> {
    let RunTarget {
        agent_type,
        repo,
        state,
        base_url,
    } = resolve_run_target(args)?;
    let bridge = supervisor_binary();

    // **§10's ownership move, completed.** The table moves the socket exactly once — M1 *"the
    // `marion` process itself"*, M2+ *"a detached `marion-supervisor`, which `marion` starts on
    // demand"* — and `marion run` is §5.7's *"first client that dials the §2 socket path and finds
    // nothing listening"*. Holding the returned connection for the rest of this function is what
    // makes that literally true: while a run is in progress the supervisor has one client, and
    // §5.7's absolute zero-client exit predicate is satisfied only after the run ends.
    //
    // **And since §11 item 28 step 6, who drives the root has moved with it.** The paragraph that
    // stood here said the opposite in as many words — *"the supervisor started below tails the
    // journal. It holds no node's `Child`, no pid it spawned, no pipe and no channel"* — and
    // concluded that *"nothing in this file may be read as delivering §7.3.1's invariant"*. Every
    // clause of that is now false. The supervisor holds the root's thread, its `Child`, its pipe
    // and its pid; `Spawned` carries that pid and is journaled at the instant the process exists
    // rather than after the turn returns; and SIGKILLing **this** process now kills a client. The
    // node keeps running, keeps writing `events.jsonl`, and a second client attaching to the same
    // supervisor is handed the whole tree — which is §9's M2 criterion 1, and
    // `tests/client_run.rs` is where it is measured.
    //
    // **And one thing moved with it that step 6's design did not name: the root's process
    // environment.** The root is `fork`/`exec`ed by the supervisor, so it inherits the
    // supervisor's environment — and the supervisor is started by whichever `marion run` arrived
    // first for this project and then outlives it (§5.7's idle grace). So a `PATH`, a proxy
    // variable or a shim an operator exports for *this* invocation reaches the root only if this
    // invocation is also the one that started the supervisor. `marion run` still passes `--auth`,
    // `--base-url` and `--repo` explicitly for exactly this reason, and everything else that used
    // to arrive by inheritance no longer does. This is a real narrowing, not a detail:
    // `tests/concurrent_projects.rs`'s same-repo bed had to stop shipping a per-run stub on
    // `PATH` because of it, and `tasks/todo.md`'s finding (d) is where it is filed for a decision.
    //
    // What this file does from here is: ensure the supervisor, send `agent/spawn` with
    // `caller: None`, attach to the root it names, and render. Two processes writing one journal is
    // unchanged and was never a compromise introduced here: `journal.rs` already states that
    // *"`marion run` and each `marion-supervisor mcp` bridge are separate processes that both cause
    // lifecycle events, so the file has concurrent writers by construction"*, serialised by
    // `O_APPEND` at record granularity — and after step 6 this process writes none of them.
    //
    // **A supervisor that will not start is now a refusal, and that is step 6's other half.** The
    // comment here used to say *"reported, not fatal — today"*, on the ground that *"the root is
    // still driven in-process, so a missing supervisor costs the socket and not the work"*, and it
    // named the test that pinned it so that whoever changed the behaviour had to say so. This is
    // that change, said out loud: the work **is** the socket now, there is deliberately no
    // in-process fallback (see [`supervisor_unreachable`]), and the test that pinned the old
    // behaviour is `run_stream.rs`'s — rewritten, not deleted, into
    // `a_run_whose_supervisor_cannot_start_is_refused_and_journals_nothing`.
    //
    // **§2's key, for the socket and the journal both, and it is the git common dir.** *"Both the
    // supervisor and its state are keyed on the project root (git common-dir, falling back to
    // cwd) — not cwd, since worktree children (§6.6) have different cwds and would otherwise hash
    // to different supervisors."* Two rules read out of one sentence: the socket is `resolve`'s and
    // the journal is `ProjectDir`'s, and until now `marion run` gave them different arguments.
    //
    // What that cost is not hypothetical. In a linked worktree `/r-wt` of `/r`, `marion run --repo
    // /r-wt` hashed `/r-wt` while `socket::resolve(/r-wt)` — which is what a bridge or a TUI calls
    // — hashes `/r/.git`, and `marion run --repo /r` hashed `/r`: three keys, so the main and the
    // linked worktree got different supervisors over different journals, which is the exact case
    // §2's rule names. Submodules divide the same way through `.git/modules/…`.
    //
    // `project_root` is applied here, in `root::prepare` and in the bridge's `spawn_env`, so all
    // three agree; `repo` itself stays the root's cwd and the base of §6.6's worktrees, which is a
    // different question the key was never answering. It is also what this client now **sends**:
    // `caller: None` requires it, because one supervisor serves every linked worktree of one
    // repository and only the client knows which of them the operator meant.
    let project_key = socket::project_root(&repo);
    let sock = socket::socket_paths(&state, &project_key, socket::own_uid());
    // **Resolved once and spent twice**, on the supervisor this run may start and on the root it
    // prepares. Two literals here would let a `--canned` run start an `Inherited` supervisor, whose
    // children would then reach the vendor directly while the root talked to the canned endpoint —
    // and nothing would say so.
    let auth = args.backend.auth();
    let launch = detach::Launch {
        program: bridge.clone(),
        state_dir: state.clone(),
        project_root: project_key.clone(),
        // **§5.7's own default, because the grace is not this client's to choose.** `marion
        // run` used to pass zero on the argument that it knows no successor is coming from
        // *it*. That argument is about one client and the grace is a property of the
        // supervisor: whichever client happens to *start* one fixes the number for every later
        // client, so a TUI attaching to a run's supervisor inherited a zero it never asked for
        // and identical behaviour depended on a startup race. §5.7 chose 300 s for precisely
        // the case that produced — *"an operator closing one window to open another"* — and a
        // run that ends moments before a TUI attaches is that case.
        //
        // It does not make this run's supervisor linger: the run's exit is an explicit
        // `session/quit` (see [`SupervisorSession`]), and §5.7's grace is what bridges between
        // clients that did **not** say they were leaving. `Handle::idle_exit_grace_waived` is
        // where that distinction is spent.
        idle_grace: RUN_IDLE_GRACE,
        // Carried onto the supervisor's argv rather than left to its environment: see
        // `detach::Launch::auth`. `resolve_base_url` already makes these a pair — `--canned`
        // yields an endpoint and nothing else does — which is the same pairing `parse_serve`
        // re-checks on the far side.
        auth,
        base_url: base_url.clone(),
    };
    let mut supervisor = connect_run_supervisor(&sock, &launch)?;

    // The bound the node will run under, resolved with the **same** function the supervisor
    // resolves it with. Not sent as a number and not binding here: the wire carries `--timeout`
    // verbatim as an `Option`, and this is only so the sentence below can name the seconds the node
    // was actually killed on.
    let blocked_bound = StdDuration::from_secs(blocked_bound_secs(
        args.timeout_secs,
        agent_type.timeout.0.as_secs(),
    ));
    let project = marion_core::paths::ProjectDir::new(&state, &project_key);

    // **The live view, on stderr.** Deliberately not stdout, and checked rather than assumed:
    // marion's stdout is a machine surface with a live consumer — `tests/launch_only_root.rs` reads
    // it back with `adapter.marion_tool_calls(&run.stdout)`, and says why in as many words ("the
    // frame it emitted must survive onto marion's own stdout, still readable as the marion call it
    // was"). A root has no `TaskContract` to return (§9), so that frame stream *is* its result, and
    // prose interleaved into it would be prose in somebody's parse. stderr already carries every
    // other line marion says to a person — the banner, the denials, the refusals — so the stream
    // joins them, and `2>/dev/null` still leaves clean frames on stdout.
    //
    // Errors are dropped rather than escalated: a closed stderr must not be what ends a run that is
    // otherwise working, and there is nowhere left to report it to anyway.
    // **Two writers, one terminal.** See [`Terminal`].
    let terminal = std::sync::Arc::new(Terminal::new(io::stderr()));

    // The journal's first production reader (§4.2, §10). Started **before** the spawn, from the
    // journal's current end, so this run's own children are the only news it can report.
    //
    // **Still a file tail, and deliberately not `tree/subscribe`.** The children are not this
    // process's to own — since step 5 the root's bridge asks the supervisor for them and the
    // supervisor runs them — and the journal is what both a watching client and a restarting
    // supervisor read. Steps 5 and 6 changed only *who writes* the records this tails; the reader is
    // unmoved, which is `events.rs`'s argument for a file cursor holding whoever the writer is.
    let stop = std::sync::Arc::new(marion_supervisor::wake::Flag::new());
    let (poller, poller_id) = tail_children(project.journal(), &terminal, &stop);

    let spawned = match spawn_root(&mut supervisor, args, &repo) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            stop.store(true, Ordering::Relaxed);
            drop(poller_id);
            let _ = poller.join();
            return Err(ExitCode::FAILURE);
        }
    };
    let root_id = spawned.agent_id.clone();
    let _ = poller_id.send(root_id.clone());

    // **A paned root is started and then attached to, and this client does neither of the two
    // things `watch_the_root` exists to do.**
    //
    // There is nothing here to render: a TUI emits bytes, not frames, so a paned node's
    // `events.jsonl` holds the two lifecycle bookends and nothing between them. Blocking for the
    // node's whole life to print "it started" and "it ended" would be merely useless.
    //
    // What makes it *harmful* is the keyboard. `node/attach` leases the node's write half to the
    // first client that asks (§5.3: one writer, and the refusal names the holder), so this
    // process — which has no terminal and cannot type — would hold it for the run's entire life,
    // and the `marion attach` the operator is about to run would be told, correctly and
    // uselessly, that connection 1 is typing into this node. It was.
    //
    // So the run returns the moment the node exists, which is exactly what the supervisor owning
    // the node's lifecycle means (§11 item 28 step 6): the node outlives this call.
    if root_follow(args.pane) == RootFollow::Started {
        stop.store(true, Ordering::Relaxed);
        let _ = poller.join();
        eprintln!("{}", pane_started_line(&root_id, &args.agent_type));
        return Ok(ExitCode::SUCCESS);
    }
    // `--detach`: the same return for a headless root. The supervisor owns the node either way;
    // this client only chose not to watch it.
    if args.detach {
        stop.store(true, Ordering::Relaxed);
        let _ = poller.join();
        println!("{}", detached_started_line(&root_id, &args.agent_type));
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!(
        "marion: root {} ({}) in {}",
        root_id.0,
        args.agent_type,
        project.agent(&root_id).path().display()
    );

    // **§7.3.3's attach, and it is the whole of how this client sees the run.** One cursor over the
    // node's `events.jsonl`: the replay leg arrives as notifications *before* the answer, and the
    // live leg continues on the same connection with the same ordinals, so there is no seam to get
    // wrong (`events.rs`). Nothing below re-reads the file — an event this run renders is an event
    // the supervisor sent.
    let watched = watch_the_root(&mut supervisor, &root_id, &terminal);
    stop.store(true, Ordering::Relaxed);
    let _ = poller.join();
    report_watched(watched, &project.journal(), &root_id, blocked_bound)
}

/// **What a client does once its root exists** — one rule, read by `run` and `resume` both.
///
/// A headless root is watched to its exit: the node's stream is the whole of how this client sees
/// the run, and its terminal bookend is the exit code. A paned root is *started* and left alone —
/// this process has no terminal and cannot type, so watching it would hold the node's one write
/// lease (§5.3) against the `marion attach` the operator is about to run. Two verbs deciding this
/// separately is how `resume` came to hand a headless root to the attach path, which refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootFollow {
    /// Say it started and return; `marion attach` is the view.
    Started,
    /// Attach to the node's stream and render it until its terminal bookend.
    Watch,
}

fn root_follow(pane: bool) -> RootFollow {
    if pane {
        RootFollow::Started
    } else {
        RootFollow::Watch
    }
}

/// One line for a paned root: what started, and the three commands that matter next.
fn pane_started_line(root_id: &marion_core::contract::AgentId, agent_type: &str) -> String {
    format!(
        "marion: root {} ({}, pane) started; `marion attach {}` to open, `^] d` to detach, \
         `marion ls` for the forest",
        root_id.0, agent_type, root_id.0
    )
}

/// One line for a root started with `--detach`, on stdout so a script can read the id: what
/// started and the commands that watch, steer and end it.
fn detached_started_line(root_id: &marion_core::contract::AgentId, agent_type: &str) -> String {
    format!(
        "marion: root {} ({}) started, detached; `marion ls` to watch, `marion steer {} <text>` \
         to steer, `marion cancel {}` to end it",
        root_id.0,
        agent_type,
        marion_supervisor::tree::short_id(&root_id.0),
        marion_supervisor::tree::short_id(&root_id.0)
    )
}

/// The end of a watched run: the transcript on stdout, the denials on stderr, and the verdict.
fn report_watched(
    watched: Result<Watched, String>,
    journal: &Path,
    root_id: &marion_core::contract::AgentId,
    blocked_bound: StdDuration,
) -> Result<ExitCode, ExitCode> {
    let watched = match watched {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{e}");
            return Err(ExitCode::FAILURE);
        }
    };
    // A closed stdout (`marion run … | head -1`) is the reader's decision, not a failure.
    let _ = write_transcript(
        &watched.transcript,
        &mut io::stdout().lock(),
        io::stdout().is_terminal(),
    );
    // Every permission marion refused this root, read back from the journal it was recorded in
    // (§9: a root has no contract, so the journal is the only place these live). Best effort by
    // construction: it is a summary of a fact the node was already told, printed in its transcript
    // above, and a journal this reader could not parse must not turn a finished run into a failure.
    for (tool, _reason) in root_denials(journal, root_id) {
        // Not "the bound expired": since `duplex::decided_permission` a root's `report` is denied
        // on arrival by §5.4 and no bound is spent, and this line has only the tool name to go on.
        // The reason the *node* was given is in the transcript printed above, which is where a
        // reader who needs the specific rule should look.
        eprintln!(
            "marion: denied {tool}: marion has nobody to ask for permission, and does not grant \
             what it cannot ask about"
        );
    }
    Ok(run_verdict(watched.terminal, blocked_bound))
}

/// The root's frames, one JSON line each, onto `out` — **unless `out` is a terminal.**
///
/// stdout is `marion run`'s machine surface (`tests/launch_only_root.rs` parses it), so a pipe gets
/// every frame. A terminal already watched the same frames rendered on stderr by the live view;
/// printing them again raw dumped ~20 KB of stream-json over the screen when a picker run ended.
fn write_transcript(frames: &[Value], out: &mut dyn Write, is_terminal: bool) -> io::Result<()> {
    if is_terminal {
        return Ok(());
    }
    for frame in frames {
        writeln!(out, "{frame}")?;
    }
    Ok(())
}

/// Dial the supervisor this run may start, and split its connection for reading. Either failure
/// is [`supervisor_unreachable`]'s sentence and a refusal.
fn connect_run_supervisor(
    sock: &socket::SocketPaths,
    launch: &detach::Launch,
) -> Result<SupervisorSession, ExitCode> {
    connect_supervisor(sock, launch).map(|(session, _started)| session)
}

/// [`connect_run_supervisor`], also answering whether this command is what started the
/// supervisor — which `marion resume` needs, because a resume that fails must not leave behind a
/// supervisor the operator did not have before.
fn connect_supervisor(
    sock: &socket::SocketPaths,
    launch: &detach::Launch,
) -> Result<(SupervisorSession, bool), ExitCode> {
    match detach::ensure_supervisor(sock, launch) {
        Ok(ensured) => {
            let lines = match ensured.stream.try_clone() {
                Ok(half) => io::BufReader::new(half),
                Err(e) => {
                    eprintln!(
                        "{}",
                        supervisor_unreachable(
                            sock.socket(),
                            &format!("its connection could not be split for reading: {e}")
                        )
                    );
                    return Err(ExitCode::FAILURE);
                }
            };
            let session = SupervisorSession {
                stream: ensured.stream,
                lines,
                socket: sock.socket().to_path_buf(),
                next_id: 1,
            };
            Ok((session, ensured.started))
        }
        // **A refusal, and the whole reason step 6 had to change this line.** See
        // [`supervisor_unreachable`]: the root is not driven in this process any more, so there is
        // nothing left for a missing supervisor to cost *except* the work.
        Err(e) => {
            eprintln!("{}", supervisor_unreachable(sock.socket(), &e.to_string()));
            Err(ExitCode::FAILURE)
        }
    }
}

/// Start the thread that tails the journal for this run's children and renders them on
/// `terminal`, and answer it with the channel its root id arrives on.
///
/// The root is excluded by id, and its id does not exist yet — so the watch is handed a channel
/// to learn it on rather than the value. Until it arrives nothing this run caused is in the file
/// at all, because the first record of the run *is* the root's own intent.
fn tail_children(
    journal: PathBuf,
    terminal: &std::sync::Arc<Terminal<io::Stderr>>,
    stop: &std::sync::Arc<marion_supervisor::wake::Flag>,
) -> (
    std::thread::JoinHandle<()>,
    std::sync::mpsc::Sender<marion_core::contract::AgentId>,
) {
    let terminal = std::sync::Arc::clone(terminal);
    let stop = std::sync::Arc::clone(stop);
    let (id_tx, id_rx) = std::sync::mpsc::channel::<marion_core::contract::AgentId>();
    // **Measured here, on the caller's thread, before the spawn** — which is what the paragraph
    // above claims and what a length read inside the reader thread does not deliver. The id
    // arrives only once `spawn_root` has returned, and by then the supervisor has launched the
    // harness: a cursor taken at that moment starts *past* whatever the harness has already
    // appended, so a corrupt record written early in the run sits behind the view and it goes
    // quiet without saying why. The end is a fact about the moment before the run existed.
    let end = marion_supervisor::watch::JournalEnd::measure(&journal);
    let handle = std::thread::spawn(move || {
        let root_id = match id_rx.recv() {
            Ok(id) => id,
            // The spawn never got an id, so there is nothing to watch and nothing to report.
            Err(_) => return,
        };
        let mut watch = JournalWatch::from_end(&journal, end, root_id);
        let file = std::cell::RefCell::new(marion_supervisor::wake::Watch::new(&journal));
        follow_journal(
            &mut watch,
            &|| stop.load(Ordering::Relaxed),
            &|| nap_until_the_journal_changes(&file, &stop, JOURNAL_POLL),
            &mut |event| {
                terminal.show(&|w| {
                    let _ = render_child(event, w);
                })
            },
        );
    });
    (handle, id_tx)
}

/// **§11 item 28 step 6: the root is created over the socket, not in this process.**
///
/// `caller: None` is what makes this a root — see `marion_core::proto::AgentSpawnParams`. The `repo`
/// is required with it and is computed by the caller because only this client knows which of the
/// trees one supervisor serves the operator meant (§2 keys the supervisor on the git common dir,
/// so `<state>/<project-hash>` names the repository and every linked worktree of it at once).
fn spawn_root(
    supervisor: &mut SupervisorSession,
    args: &Args,
    repo: &Path,
) -> Result<marion_core::proto::result::AgentSpawnResult, String> {
    let id = supervisor.send(marion_core::proto::Call::AgentSpawn(
        marion_core::proto::params::AgentSpawnParams {
            review_of: None,
            notify_parent: false,
            agent_type: args.agent_type.clone(),
            prompt: args.prompt.clone(),
            native_launch: None,
            caller: None,
            repo: Some(repo.to_path_buf()),
            // A root states none: §9's contract terms belong to a child's `spawn`, and a root
            // has no contract to carry them.
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec![],
            // Stated as the operator stated it. The supervisor resolves it, so a number
            // invented here would be a second source of truth for §3.1's own key.
            timeout_secs: args.timeout_secs,
            model: args.model.clone(),
            // Root-only, and stated rather than defaulted so the supervisor can tell an
            // operator who declined the snapshot from one who said nothing.
            no_change_record: Some(args.no_change_record),
            // **Stated only when asked for**, exactly as `no_change_record` is: absent and
            // `false` must stay distinguishable on the wire, or the supervisor's root-only
            // pairing refusal would fire on every operator who never mentioned a pane.
            pane: args.pane.then_some(true),
            // **Child-only, and the supervisor refuses either of them beside `caller: None`.**
            // A root's workspace is not a choice: it is the operator's own checkout at `repo`,
            // which is what §9's change record measures. `marion run` therefore has no
            // `--isolation` flag to forward and states neither field.
            isolation: None,
            allow_concurrent_writes: None,
            // The operator's choice of login, where they stated one; the supervisor resolves the
            // agent type's and the default otherwise.
            profile: args.profile.clone(),
        },
    ))?;
    // Nothing can be notified before the first attach, so the sink here is unreachable — and it
    // is a real sink rather than a panic, because a supervisor speaking early is not a reason
    // to lose a run.
    let outcome = supervisor.pump(Awaited::Response(id), &mut |_| {})?;
    match outcome {
        marion_core::proto::Outcome::Result(body) => {
            match marion_core::proto::Method::AgentSpawn.decode_result(&body) {
                Ok(marion_core::proto::MethodResult::AgentSpawn(r)) => Ok(r),
                _ => Err(
                    "marion: this project's supervisor answered `agent/spawn` with a \
                          result marion cannot read"
                        .to_string(),
                ),
            }
        }
        // The supervisor's own sentence, verbatim. It already names the rule and the field;
        // re-wording it here would put marion's guess in front of marion's answer.
        marion_core::proto::Outcome::Error(e) => Err(format!("marion: {}", e.message)),
    }
}

/// The run's exit code, read off how the root's stream ended.
fn run_verdict(terminal: Terminal_, blocked_bound: StdDuration) -> ExitCode {
    match terminal {
        Terminal_::Aborted(reason) => {
            eprintln!("marion: {reason}");
            ExitCode::FAILURE
        }
        Terminal_::Exited { status, exit } => {
            if status == ExitStatus::TimedOut {
                eprintln!(
                    "marion: the root exceeded its {} s wall-clock bound and its process group \
                     was killed",
                    blocked_bound.as_secs()
                );
                return ExitCode::FAILURE;
            }
            // **The status decides, and the exit code only refines it.** A root that marion
            // refused on §6.1 step 8 exited **zero** — that is the whole failure shape
            // `RootError::BridgeNeverReached` exists to catch: a harness that took its turn
            // without marion's tools, ended as plain text, and claimed success. Reading `code`
            // first would hand exactly that run back to the operator as `ExitCode::SUCCESS`,
            // which is the silence §6.1 step 8 refuses, restored by the client.
            if status != ExitStatus::Ok {
                // marion's own sentence for this ending, carried on the node's closing bookend
                // and printed because it is the only place a client can now read it — see
                // `root::bridge_never_reached_exit`.
                eprintln!("marion: {}", exit.description);
                return ExitCode::FAILURE;
            }
            // A root that answered without delegating is a success, and says so rather than
            // ending in silence (`root::ANSWERED_WITHOUT_DELEGATING`).
            if exit.description.contains(root::ANSWERED_WITHOUT_DELEGATING) {
                eprintln!("marion: note: {}", root::ANSWERED_WITHOUT_DELEGATING);
            }
            match exit.code {
                Some(0) => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
    }
}

/// How the root's stream ended, as the client read it off the wire.
///
/// The two arms are `events.jsonl`'s own bookends (`marion_core::event::Lifecycle`), not a second
/// vocabulary: `Exited` is a node whose process ran and stopped, `Aborted` is marion's own sentence
/// for a launch that never got there. Reading the verdict off the node's stream rather than off the
/// journal is what keeps this client working when the journal is the thing that went bad — which
/// `run_stream.rs` measures, and which a viewer may never turn into a failed run.
enum Terminal_ {
    Exited {
        status: ExitStatus,
        /// The whole `ProcessExit`, **description included**. Keeping only the code was a real
        /// loss: marion's sentence for *why* an ending was refused rides on that field, and the
        /// socket is now the only channel it travels.
        exit: marion_core::contract::ProcessExit,
    },
    Aborted(String),
}

/// What one attached run produced.
struct Watched {
    /// Every frame the root emitted, parsed — the machine surface `marion run` prints to stdout.
    transcript: Vec<Value>,
    terminal: Terminal_,
}

/// Attach to the root and render its stream until it ends.
fn watch_the_root(
    supervisor: &mut SupervisorSession,
    root_id: &marion_core::contract::AgentId,
    terminal: &Terminal<io::Stderr>,
) -> Result<Watched, String> {
    /// The render loop's own state, as a value rather than as a closure's captures.
    ///
    /// A closure holding both would keep `ended` mutably borrowed for the whole live loop, which
    /// cannot then ask whether the run is over. One `&mut self` method reads and writes the same
    /// state and the loop reads it between calls.
    struct Rendering<'a> {
        root_id: &'a marion_core::contract::AgentId,
        terminal: &'a Terminal<io::Stderr>,
        transcript: Vec<Value>,
        ended: Option<Terminal_>,
        view: LiveView,
    }

    impl Rendering<'_> {
        /// One event through the live view, then to the terminal in one write. Rendered into a
        /// buffer first because the view holds state across frames and `Terminal::show` takes a
        /// `Fn`.
        fn show(&mut self, event: StreamEvent<'_>) {
            let mut buf: Vec<u8> = Vec::new();
            let _ = self.view.event(event, &mut buf);
            self.write(&buf);
        }

        /// The held chunk text, written: the run's end is the last frame it will get.
        fn flush(&mut self) {
            let mut buf: Vec<u8> = Vec::new();
            let _ = self.view.flush(&mut buf);
            self.write(&buf);
        }

        fn write(&self, buf: &[u8]) {
            if !buf.is_empty() {
                self.terminal.show(&|w| {
                    let _ = w.write_all(buf);
                });
            }
        }

        fn note(&mut self, event: marion_core::proto::notify::Event) {
            use marion_core::event::{Lifecycle, Payload};
            let marion_core::proto::notify::Event::NodeEvent {
                agent_id, payload, ..
            } = &event
            else {
                // `tree/node-added` and `node/state` legitimately interleave; this client renders
                // children from the journal, so they are not its news.
                return;
            };
            if agent_id != self.root_id {
                return;
            }
            let Ok(payload) = serde_json::from_value::<Payload>(payload.clone()) else {
                return;
            };
            match payload {
                Payload::Vendor { json, .. } => {
                    self.show(StreamEvent::Frame(&json));
                    self.transcript.push(json);
                }
                Payload::Raw(line) => self.show(StreamEvent::Unparsed(&line)),
                // **Said, not skipped.** These are frames marion read and did not keep whole — a
                // §5.2 withholding, or a payload past `MAX_EVENT_BYTES`. Rendering them as silence
                // would make a shortened stream indistinguishable from a quiet one, which is the
                // failure `marion_core::event::Payload` has separate variants to prevent.
                // The *reason* is deliberately not printed with it. It is a paragraph — S9's
                // measurement of what a Claude Code session catalogue contains — and this view
                // holds every rendered line inside one terminal width (`run_stream.rs` asserts the
                // bound). It is not lost: `events.jsonl` carries it on the record, which is where a
                // reader who wants to know *which* rule looks.
                Payload::Withheld { key, bytes, .. } => self.show(StreamEvent::Unparsed(&format!(
                    "[withheld: a `{key}` frame of {bytes} bytes, body not kept]"
                ))),
                Payload::Oversized { was, bytes } => self.show(StreamEvent::Unparsed(&format!(
                    "[marion shortened a {was:?} frame of {bytes} bytes]"
                ))),
                // Uninhabited (`marion_core::event::Normalization`), so this arm cannot be reached
                // and is here so that adding the first normalized payload is a compile error in
                // every renderer rather than a frame that silently disappears from one.
                Payload::Normalized(_) => {}
                // The opening bookend says marion began recording; the banner already said the run
                // started, so it adds nothing a person reads.
                Payload::Lifecycle(Lifecycle::Opened) => {}
                Payload::Lifecycle(Lifecycle::Exited { status, exit }) => {
                    self.flush();
                    self.ended = Some(Terminal_::Exited { status, exit });
                }
                Payload::Lifecycle(Lifecycle::Aborted { reason }) => {
                    self.flush();
                    self.ended = Some(Terminal_::Aborted(reason));
                }
            }
        }
    }

    let mut r = Rendering {
        root_id,
        terminal,
        transcript: Vec::new(),
        ended: None,
        view: LiveView::default(),
    };

    let id = supervisor.send(marion_core::proto::Call::NodeAttach(
        marion_core::proto::params::NodeAttachParams {
            agent_id: root_id.clone(),
            pane_stream: None,
        },
    ))?;
    let outcome = supervisor.pump(Awaited::Response(id), &mut |e| r.note(e))?;
    match outcome {
        marion_core::proto::Outcome::Result(body) => {
            match marion_core::proto::Method::NodeAttach.decode_result(&body) {
                Ok(marion_core::proto::MethodResult::NodeAttach(attached)) => {
                    // A node that already reached a terminal reading is `ReplayOnly`, and the
                    // bookend for it is already in the replay above. Anything else is live and
                    // the loop below is what reads it.
                    let _ = attached;
                }
                _ => {
                    return Err(
                        "marion: this project's supervisor answered `node/attach` with a \
                                result marion cannot read"
                            .to_string(),
                    );
                }
            }
        }
        marion_core::proto::Outcome::Error(e) => return Err(format!("marion: {}", e.message)),
    }

    // The live leg. **No bound of its own**: the node's bound is the supervisor's to keep, and a
    // second deadline here would kill the view of a run that was still inside the first one. What
    // ends this loop is the node's terminal bookend, or the connection going away — which
    // `next_frame` reports as the refusal it is rather than as an end.
    while r.ended.is_none() {
        match supervisor.next_frame()? {
            marion_core::proto::Frame::Notification(n) => r.note(n.event),
            other => {
                return Err(format!(
                    "marion: this project's supervisor sent an unexpected frame while the root was \
                     running: {other:?}"
                ));
            }
        }
    }
    Ok(Watched {
        transcript: r.transcript,
        terminal: r
            .ended
            .expect("the loop exits only once a terminal bookend arrived"),
    })
}

/// Every permission marion denied this root, `(tool, reason)`, read out of the journal.
///
/// **A summary and never a verdict.** Empty is returned for a journal that is missing, unreadable
/// or corrupt, because that is a viewer's problem and `run_stream.rs` pins the rule this obeys: a
/// view that cannot read the file must not end the run. The denials themselves are not lost either
/// way — each one was delivered to the node and appears in the transcript.
fn root_denials(journal: &Path, root_id: &marion_core::contract::AgentId) -> Vec<(String, String)> {
    let Ok(bytes) = std::fs::read(journal) else {
        return Vec::new();
    };
    let mut replay = marion_core::registry::Replay::default();
    replay.extend(&bytes);
    replay
        .nodes()
        .iter()
        .find(|n| &n.agent_id == root_id)
        .map(|n| {
            n.denied_permissions
                .iter()
                .map(|d| (d.tool.clone(), d.reason.clone()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {

    /// **Raw frames reach stdout only when stdout is not a terminal.** The live view already
    /// rendered every frame on stderr; printing the transcript again on a terminal dumped ~20 KB of
    /// stream-json over it when the picker's run ended. Piped, stdout stays the machine surface.
    #[test]
    fn the_transcript_is_written_to_a_pipe_and_never_to_a_terminal() {
        let frames = vec![serde_json::json!({"type": "result"})];
        let mut piped = Vec::new();
        write_transcript(&frames, &mut piped, false).unwrap();
        assert_eq!(String::from_utf8(piped).unwrap(), "{\"type\":\"result\"}\n");
        let mut tty = Vec::new();
        write_transcript(&frames, &mut tty, true).unwrap();
        assert!(tty.is_empty());
    }

    /// **`--canned` against a port nobody listens on is refused at once, naming how to start the
    /// provider**, instead of the run hanging forever on a provider that will never answer (the
    /// README's first command did exactly that). A listening port passes.
    #[test]
    fn a_canned_endpoint_that_is_not_listening_is_refused_fast_with_the_command_to_start_one() {
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        let url = format!("http://127.0.0.1:{port}/v1");
        let started = std::time::Instant::now();
        let e = canned_endpoint_listening(&url).expect_err("nothing listens there");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "bounded"
        );
        assert!(e.contains(&url), "{e}");
        assert!(
            e.contains("marion-canned"),
            "it names the provider binary: {e}"
        );
        assert!(e.contains("MARION_CANNED_PORT"), "and how to aim it: {e}");

        let live = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/v1", live.local_addr().unwrap().port());
        assert_eq!(canned_endpoint_listening(&url), Ok(()));
        assert!(
            canned_endpoint_listening("not a url").is_err(),
            "an unreadable endpoint is refused, not probed as a guess"
        );
    }

    fn words(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// **Every command is one row of the table, and every row is reachable by its word** — the
    /// dispatch's whole contract. An old spelling reaches its command and is listed nowhere; a
    /// word that is no command is not guessed at.
    #[test]
    fn every_command_is_a_row_and_its_old_spellings_reach_it() {
        for v in cli::VERBS {
            assert_eq!(cli::verb(v.name).map(|found| found.name), Some(v.name));
        }
        assert_eq!(cli::VERBS.len(), 13, "a command was added or dropped");
        assert_eq!(cli::verb("tree").map(|v| v.name), Some("ls"));
        assert!(cli::verb("bogus").is_none());
        let top = cli::top_help();
        for v in cli::VERBS {
            match v.summary {
                Some(_) => assert!(top.contains(&format!("  {:<9}", v.name)), "{}", v.name),
                None => assert!(!top.contains(&format!("  {} ", v.name)), "{}", v.name),
            }
        }
        assert!(!top.contains("  tree"), "an old spelling is listed:\n{top}");
    }

    /// **Each command's `--help` is its own**, short enough to read, and names the command it
    /// belongs to — not one screen of every command's details.
    #[test]
    fn each_command_has_its_own_short_help() {
        for v in cli::VERBS {
            let help = (v.help)();
            assert!(help.starts_with("usage: marion "), "{}:\n{help}", v.name);
            let widest = help.lines().map(|l| l.chars().count()).max().unwrap_or(0);
            assert!(
                widest <= 92,
                "{} help runs {widest} columns wide:\n{help}",
                v.name
            );
            assert!(
                help.lines().count() <= 40,
                "{} help is a wall:\n{help}",
                v.name
            );
            assert!(
                !help.contains('§'),
                "{} help cites the spec:\n{help}",
                v.name
            );
        }
        assert!(!cli::top_help().contains('§'));
        assert!(cli::top_help().lines().count() <= 30);
    }

    /// **`--help` is a request for help anywhere before `--`, and a word after it.** So `marion
    /// steer ab12 -- --help` sends the text `--help`, and `marion run claude --prompt --help` sets
    /// the prompt: a value is taken whatever it looks like.
    #[test]
    fn help_is_asked_for_before_the_double_dash_only() {
        assert_eq!(
            parse_attach(&words(&["attach", "--help"])).err(),
            Some(Exit::Help)
        );
        assert_eq!(
            parse_attach(&words(&["attach", "a", "-h"])).err(),
            Some(Exit::Help)
        );
        let steer = parse_steer(&words(&["steer", "a", "--", "--help"])).expect("text");
        assert_eq!(steer.text, SteerText::Words("--help".into()));
        let steer = parse_steer(&words(&["steer", "a", "say", "--help"])).expect("text");
        assert_eq!(steer.text, SteerText::Words("say --help".into()));
        let run = parse_args(&words(&["run", "claude", "--prompt", "--help"])).expect("a value");
        assert_eq!(run.prompt, "--help");
        assert!(cli::asks_for_help(&words(&["add", "--help"])));
        assert!(!cli::asks_for_help(&words(&["--", "--help"])));
    }

    /// **`--flag=value` is the same as `--flag value`**, and a switch given a value is refused
    /// rather than read as `true`.
    #[test]
    fn a_flag_value_may_be_joined_with_an_equals_sign() {
        let run = parse_args(&words(&["run", "claude", "--prompt=go", "--timeout=30"])).unwrap();
        assert_eq!((run.prompt.as_str(), run.timeout_secs), ("go", Some(30)));
        let attach = parse_attach(&words(&["attach", "a", "--repo=/r"])).unwrap();
        assert_eq!(attach.place.repo.as_deref(), Some(Path::new("/r")));
        assert!(parse_args(&words(&["run", "claude", "--prompt", "x", "--pane=yes"])).is_err());
    }

    /// **A usage refusal says what was wrong**, in plain words, rather than printing the help.
    #[test]
    fn a_usage_refusal_names_what_was_wrong() {
        let why = |argv: &[&str]| match parse_args(&words(argv)) {
            Err(Exit::Usage(why)) => why,
            other => panic!("{argv:?} parsed: {:?}", other.map(|a| a.prompt)),
        };
        assert!(why(&["run"]).contains("agent type"));
        assert!(why(&["run", "claude"]).contains("--prompt"));
        assert!(why(&["run", "claude", "--prompt", "x", "--bogus"]).contains("--bogus"));
        assert!(why(&["run", "claude", "--prompt"]).contains("needs a value"));
        assert!(why(&["run", "claude", "--prompt", "x", "--timeout", "soon"]).contains("seconds"));
        assert!(why(&["run", "claude", "fix", "it"]).contains("--prompt"));
    }

    /// The two flags an attach shares with a run are the two that answer *which supervisor* — §2
    /// keys one on the git common dir, so an attach that resolved a different project would ask a
    /// supervisor that has never heard of the node.
    #[test]
    fn attach_takes_an_id_and_the_two_flags_that_choose_a_supervisor() {
        let got = parse_attach(&words(&["attach", "a-1"])).expect("`marion attach a-1` parses");
        assert_eq!(got.agent_id, "a-1");
        assert_eq!(got.place, Place::default());
        let got = parse_attach(&words(&[
            "attach",
            "a-1",
            "--repo",
            "/r",
            "--state-dir",
            "/s",
        ]))
        .expect("both flags parse");
        assert_eq!(got.place.repo.as_deref(), Some(Path::new("/r")));
        assert_eq!(got.place.state_dir.as_deref(), Some("/s"));
    }

    /// An unknown flag is a refusal, never a silent ignore: a mistyped `--state-dir` that fell
    /// through would dial a supervisor under `$HOME` and report the operator's live node as missing.
    #[test]
    fn an_attach_flag_marion_does_not_know_is_refused_rather_than_ignored() {
        for bad in [
            vec!["attach"],
            vec!["attach", "--repo", "/r"],
            vec!["attach", "a-1", "--prompt", "go"],
            vec!["attach", "a-1", "--repo"],
            vec!["attach", "a-1", "--canned"],
            vec!["attach", "a-1", "b-2"],
        ] {
            assert!(parse_attach(&words(&bad)).is_err(), "{bad:?} was accepted");
        }
    }

    /// **`marion ls` takes an optional id, `--attention` and the two flags that choose a
    /// supervisor, and nothing else** — for `parse_attach`'s reason: a mistyped `--state-dir` that
    /// fell through would list the empty forest under `$HOME` and report every agent as absent.
    #[test]
    fn ls_takes_an_id_attention_and_the_project_flags_and_refuses_the_rest() {
        let bare = parse_list(&words(&["ls"])).expect("`marion ls` parses");
        assert_eq!(bare, ListArgs::default());
        let full = parse_list(&words(&[
            "list",
            "a-1",
            "--attention",
            "--repo",
            "/r",
            "--state-dir",
            "/s",
        ]))
        .expect("every flag parses");
        assert_eq!(full.target.as_deref(), Some("a-1"));
        assert!(full.attention);
        assert_eq!(full.place.repo.as_deref(), Some(Path::new("/r")));
        assert_eq!(full.place.state_dir.as_deref(), Some("/s"));
        for bad in [
            vec!["ls", "a-1", "b-2"],
            vec!["ls", "--repo"],
            vec!["ls", "--state-dir"],
            vec!["ls", "--canned"],
            vec!["ls", "--attention=yes"],
        ] {
            assert!(parse_list(&words(&bad)).is_err(), "{bad:?} was accepted");
        }
    }

    /// **`marion steer <id> [--repo p] [--state-dir p] [--] <text…>`**: the project flags only
    /// before the text, the words after them joined with single spaces, and a lone `-` meaning
    /// stdin. `--` ends the flags, so a message that begins with a dash can still be sent. Every
    /// other shape is a refusal — above all a missing message, which would queue an empty turn.
    #[test]
    fn steer_takes_a_target_the_project_flags_and_a_message() {
        let plain = parse_steer(&words(&["steer", "8ea3", "use", "the", "v2", "API"]))
            .expect("`marion steer <id> <text…>` parses");
        assert_eq!(plain.target, "8ea3");
        assert_eq!(plain.text, SteerText::Words("use the v2 API".into()));
        assert_eq!(plain.place, Place::default());

        let flagged = parse_steer(&words(&[
            "steer",
            "8ea3",
            "--repo",
            "/r",
            "--state-dir",
            "/s",
            "--",
            "--repo",
            "is",
            "text",
        ]))
        .expect("flags, then `--`, then text");
        assert_eq!(flagged.place.repo.as_deref(), Some(Path::new("/r")));
        assert_eq!(flagged.place.state_dir.as_deref(), Some("/s"));
        assert_eq!(flagged.text, SteerText::Words("--repo is text".into()));

        let piped = parse_steer(&words(&["steer", "8ea3", "-"])).expect("`-` reads stdin");
        assert_eq!(piped.text, SteerText::Stdin);
        let quoted = parse_steer(&words(&["steer", "8ea3", "text", "-"])).expect("a dash in text");
        assert_eq!(quoted.text, SteerText::Words("text -".into()));

        for bad in [
            vec!["steer"],
            vec!["steer", "8ea3"],
            vec!["steer", "8ea3", "--"],
            vec!["steer", "--repo", "/r"],
            vec!["steer", "8ea3", "--repo"],
            vec!["steer", "8ea3", "--canned", "hi"],
            vec!["steer", "8ea3", "-", "more"],
        ] {
            assert!(parse_steer(&words(&bad)).is_err(), "{bad:?} was accepted");
        }
    }

    /// A message read from stdin loses the one newline a pipe adds, and nothing else.
    #[test]
    fn a_piped_message_loses_only_its_trailing_newline() {
        assert_eq!(piped_message("use v2\n".into()), "use v2");
        assert_eq!(piped_message("use v2\r\n".into()), "use v2");
        assert_eq!(piped_message("a\nb\n\n".into()), "a\nb\n");
        assert_eq!(piped_message("  keep  ".into()), "  keep  ");
    }

    /// Endpoint mode starts with `marion login`, so its help names `logout` and the forms a user
    /// reaches for: a label, stdin, the provider's env var, the listing, a custom provider.
    #[test]
    fn the_login_help_names_login_and_logout() {
        let text = (cli::verb("login").unwrap().help)();
        for needle in [
            "marion login <provider>[:<label>]",
            "--stdin",
            "--from-env",
            "marion login --list",
            "marion login custom <id> --base-url <url> --wire <wire>",
            "marion logout <provider>[:<label>]",
        ] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
    }

    /// **§9's M5 clause 3 needs a UI an operator can actually open**, and a tree screen reachable
    /// from no binary is the "fully tested in isolation, wired to nothing" failure this repo has
    /// shipped more than once. Discoverable (bare `marion` and `ls` are both in the help), and
    /// wired: `ls` resolves a project, so an unresolvable repo fails there.
    #[test]
    fn the_watch_screen_is_reachable_from_this_binary() {
        let text = cli::top_help();
        assert!(text.contains("home screen"), "{text}");
        assert!(text.contains("  ls "), "{text}");
        assert_eq!(
            ls_main(&words(&["ls", "--repo", "/nonexistent-marion-ls-smoke"])),
            Ok(ExitCode::FAILURE)
        );
    }
    use super::*;
    use marion_core::agent_type::builtin;

    // The shared self-removing scratch dir, rather than the tenth hand-rolled copy of one.
    //
    // **The local copy's stated reason was true when written and quietly stopped being true**, so
    // it is corrected here rather than merely deleted. It read: *"`marion` ships no dev-dependency
    // for this and one flag's default does not justify adding one."* `marion-testsupport` has
    // since become a dev-dependency of this package, and a bin target gets dev-dependencies in its
    // test build — so the helper this file hand-rolled was already reachable, and had been for a
    // while. The next person to want a temp dir here should find out it is free rather than
    // inherit a justification for writing an eleventh copy. Cargo.toml says the same thing from
    // the other end: a dev-dependency was chosen precisely so that `#[cfg(test)]` code *inside*
    // this crate could reach the guard, which is exactly this call site.
    //
    // **The `AtomicU32` went with it, deliberately.** It disambiguated a tag reused within one
    // test; the two call sites left use distinct tags once each, and `scratch` already appends the
    // pid and a thread tag. A counter kept "just in case" would be a second uniqueness scheme
    // competing with the one in the shared helper.
    //
    // **Bind the guard for the whole test.** `scratch("x").join("y")` drops it at the end of that
    // statement — see the `nogit` test below, which was written in exactly that shape back when it
    // was harmless.
    use marion_testsupport::scratch;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    // --- the live view ---------------------------------------------------------------------------

    /// One frame through the renderer, as the lines a person would see.
    fn shown(frame: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(frame).expect("the fixture frame parses");
        let mut buf: Vec<u8> = Vec::new();
        render_event(StreamEvent::Frame(&v), &mut buf).expect("rendering a frame cannot fail");
        String::from_utf8(buf)
            .expect("the renderer writes UTF-8")
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    /// **A root's answer is shown as its words**, on the item-stream shape too (codex `exec
    /// --json`): `marion run codex --prompt "say hello"` rendered "frame item.completed" and never
    /// the "Hello!" the root said. Other item kinds keep their one-line name.
    #[test]
    fn an_item_stream_message_is_rendered_as_the_roots_words() {
        assert_eq!(
            shown(
                r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"Hello!"}}"#
            ),
            ["root      Hello!"]
        );
        assert_eq!(
            shown(r#"{"type":"item.completed","item":{"id":"i","type":"reasoning","text":"abc"}}"#),
            ["think     (3 characters of reasoning)"]
        );
        assert_eq!(
            shown(r#"{"type":"turn.started"}"#),
            ["frame     turn.started"]
        );
    }

    /// The event a person runs `marion run` to watch: the root stopping working alone. It is marked
    /// differently from every other tool call, and its arguments are readable rather than JSON.
    #[test]
    fn a_spawn_is_marked_as_marions_own_verb_and_a_builtin_tool_is_not() {
        let lines = shown(
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"toolu_1","name":"mcp__marion__spawn",
                 "input":{"agent_type":"codex","prompt":"fix the failing test","timeout_secs":600}}]}}"#,
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("MARION"), "{:?}", lines[0]);
        assert!(lines[0].contains("spawn"), "{:?}", lines[0]);
        assert!(lines[0].contains(r#"agent_type="codex""#), "{:?}", lines[0]);
        assert!(
            lines[0].contains(r#"prompt="fix the failing test""#),
            "{:?}",
            lines[0]
        );

        let builtin = shown(
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"git status"}}]}}"#,
        );
        assert_eq!(builtin.len(), 1, "{builtin:?}");
        assert!(builtin[0].starts_with("tool"), "{:?}", builtin[0]);
        assert!(
            builtin[0].contains(r#"Bash  command="git status""#),
            "{:?}",
            builtin[0]
        );
        assert!(
            !builtin[0].contains("MARION"),
            "a built-in must not be dressed up as the interesting event: {:?}",
            builtin[0]
        );
    }

    /// Assistant prose arrives as prose, over as many lines as it has — and thinking is counted,
    /// never printed, because it is long and it is not what a watcher is here for.
    #[test]
    fn assistant_text_is_shown_as_text_and_thinking_is_only_counted() {
        let lines = shown(
            r#"{"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"twelve chars"},
                {"type":"text","text":"Delegating to a codex child.\nOne child, then I will report."}]}}"#,
        );
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("think"), "{:?}", lines[0]);
        assert!(lines[0].contains("12 characters"), "{:?}", lines[0]);
        assert!(!lines[0].contains("twelve chars"), "{:?}", lines[0]);
        assert!(lines[1].starts_with("root"), "{:?}", lines[1]);
        assert!(
            lines[1].ends_with("Delegating to a codex child."),
            "{:?}",
            lines[1]
        );
        // A continuation keeps the body column and drops the tag, so prose reads as prose.
        assert!(lines[2].starts_with("        "), "{:?}", lines[2]);
        assert!(
            lines[2].contains("One child, then I will report."),
            "{:?}",
            lines[2]
        );
    }

    /// A tool result is brief when it worked and **verbatim when it did not**: a truncated error
    /// misreports what the node said, which is the one thing worth the width.
    #[test]
    fn a_tool_result_is_brief_and_a_failure_is_verbatim() {
        let long = "x".repeat(4000);
        let ok = shown(&format!(
            r#"{{"type":"user","message":{{"content":[
                {{"type":"tool_result","tool_use_id":"t1","content":"{long}"}}]}}}}"#
        ));
        assert_eq!(ok.len(), 1, "{ok:?}");
        assert!(ok[0].starts_with("ok"), "{:?}", ok[0]);
        assert!(
            ok[0].chars().count() < 300,
            "a 4 kB tool result must not be pasted at a person: {} chars",
            ok[0].chars().count()
        );
        assert!(ok[0].ends_with('…'), "{:?}", ok[0]);

        let failed = shown(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","is_error":true,
                 "content":[{"type":"text","text":"depth 3 exceeds the gate"}]}]}}"#,
        );
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].starts_with("FAILED"), "{:?}", failed[0]);
        assert!(
            failed[0].contains("depth 3 exceeds the gate"),
            "{:?}",
            failed[0]
        );
    }

    /// The run's own verdict, and — when it is a failure — the node's words about it in full.
    #[test]
    fn the_terminal_result_reports_the_verdict_and_prints_a_failure_in_full() {
        let ok = shown(
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":12345,
                "num_turns":4,"total_cost_usd":0.0213,"result":"done"}"#,
        );
        assert_eq!(ok.len(), 1, "{ok:?}");
        assert!(ok[0].starts_with("done"), "{:?}", ok[0]);
        assert!(ok[0].contains("12.3s"), "{:?}", ok[0]);
        assert!(ok[0].contains("4 turns"), "{:?}", ok[0]);
        assert!(ok[0].contains("$0.0213"), "{:?}", ok[0]);

        let bad = shown(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,
                "num_turns":2,"result":"the provider returned 500 for the second request"}"#,
        );
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(bad[0].starts_with("FAILED"), "{:?}", bad[0]);
        assert!(bad[0].contains("error_during_execution"), "{:?}", bad[0]);
        assert!(
            bad[1].contains("the provider returned 500 for the second request"),
            "an error is printed whole: {:?}",
            bad[1]
        );
    }

    /// A permission ask is §9's dead end, and a watcher should see it happening rather than
    /// wonder why the run stopped moving for the length of the `Blocked` bound.
    #[test]
    fn a_permission_ask_says_what_marion_is_about_to_do_about_it() {
        let lines = shown(
            r#"{"type":"control_request","request_id":"u-1","request":
               {"subtype":"can_use_tool","tool_name":"mcp__marion__report"}}"#,
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("PERMIT"), "{:?}", lines[0]);
        assert!(lines[0].contains("mcp__marion__report"), "{:?}", lines[0]);
        assert!(lines[0].contains("denied"), "{:?}", lines[0]);
    }

    /// **Rule 1: no frame kind is silent, and rule 2: none is a JSON dump.**
    ///
    /// The table is every `type`/`subtype` pair the recorded fixtures contain, plus two kinds that
    /// exist nowhere — a plausible future one and a frame with no `type` at all. Each must produce
    /// at least one line, and no line may be long enough to be a paste of the frame. The 30 kB
    /// `initialize` `control_response` is in here for exactly that reason.
    #[test]
    fn every_frame_kind_including_one_nobody_has_seen_produces_a_line_and_never_a_json_dump() {
        let catalogue = &[
            r#"{"type":"system","subtype":"init","model":"claude-opus-5","tools":["Task","Bash"],
                "mcp_servers":[{"name":"marion","status":"connected"}],"cwd":"/repo"}"#,
            r#"{"type":"system","subtype":"status","status":"requesting"}"#,
            r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"system","subtype":"hook_response","hook_name":"SubagentStop","exit_code":0}"#,
            r#"{"type":"system","subtype":"task_started","task_id":"a-1","description":"probe"}"#,
            r#"{"type":"system","subtype":"task_updated","patch":{"status":"completed"}}"#,
            r#"{"type":"system","subtype":"task_notification","status":"completed","summary":"ok"}"#,
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":1}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            r#"{"type":"control_request","request_id":"d-1","request":{"subtype":"request_user_dialog"}}"#,
            // The one that would be a wall of text if it were dumped.
            &format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"marion-init-1",
                   "response":{{"commands":[{}]}}}}}}"#,
                (0..400)
                    .map(|i| format!(r#"{{"name":"cmd-{i}","description":"a slash command"}}"#))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            // Nobody has seen either of these. That is the point.
            r#"{"type":"a_kind_from_a_future_cli","subtype":"nobody_has_measured_this"}"#,
            r#"{"session_id":"no type field at all"}"#,
        ];
        for frame in catalogue {
            let lines = shown(frame);
            assert!(
                !lines.is_empty() && lines.iter().any(|l| !l.trim().is_empty()),
                "an unhandled frame kind rendered as silence, which reads exactly like nothing \
                 having happened:\n{frame}"
            );
            for line in &lines {
                assert!(
                    line.chars().count() <= 240,
                    "a rendered line is long enough to be a JSON paste ({} chars):\n{line}",
                    line.chars().count()
                );
                assert!(
                    !line.contains(r#""description":"a slash command""#),
                    "the initialize catalogue was pasted at a person:\n{line}"
                );
            }
        }
    }

    /// **The moment the whole live view exists for: a child came back.**
    ///
    /// `bridge::spawn_result` answers a `spawn` with the entire contract pretty-printed, so the
    /// generic tool-result path would render it as 200 characters of `{ "acceptance_criteria": …`
    /// — a JSON dump, at the one instant a watcher is paying attention. The three fields worth a
    /// line are which harness ran, how it ended, and what it said.
    ///
    /// The literal below is the shape `spawn_result` emits, marion's own `TaskContract` rather than
    /// a harness's frame — which is why reading specific fields is allowed here. If that shape
    /// moves, `contract_summary` returns `None` and the generic brief takes over: less detail, and
    /// never a wrong claim.
    #[test]
    fn a_returned_contract_is_a_sentence_about_the_child_and_not_a_wall_of_json() {
        let contract = r#"{
  "task_id": "019fd2b4-0000-7000-8000-000000000001",
  "requester": "019fd2b4-0000-7000-8000-000000000002",
  "child": {"harness": "codex", "version": "0.146.0", "model": null},
  "acceptance_criteria": [],
  "completion": {
    "status": "Ok",
    "narrative": {"value": "fixed the failing test and pushed one commit", "truncated": false,
                  "original_bytes": 43},
    "result_commits": ["abc1234"],
    "exit": {"description": "child exited with code 0"}
  }
}"#;
        let ok = shown(&format!(
            r#"{{"type":"user","message":{{"content":[
                {{"type":"tool_result","tool_use_id":"toolu_1","content":{}}}]}}}}"#,
            serde_json::to_string(contract).unwrap()
        ));
        assert_eq!(ok.len(), 1, "{ok:?}");
        assert!(ok[0].starts_with("CHILD"), "{:?}", ok[0]);
        assert!(ok[0].contains("the codex child finished"), "{:?}", ok[0]);
        assert!(
            ok[0].contains("fixed the failing test and pushed one commit"),
            "the narrative is what the child actually reported: {:?}",
            ok[0]
        );
        assert!(
            !ok[0].contains("task_id"),
            "the contract's plumbing belongs in the journal, not on a terminal: {:?}",
            ok[0]
        );

        // A failed child: `spawn_result` puts marion's own one-line account **above** the JSON, and
        // that line is the message — it is kept whole and shown first.
        let failed = shown(&format!(
            r#"{{"type":"user","message":{{"content":[
                {{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":{}}}]}}}}"#,
            serde_json::to_string(&format!(
                "marion: the codex child failed — child exited with code 1; the child's stream \
                 reported: This model is no longer available to new users.\n\n{}",
                contract.replace(r#""status": "Ok""#, r#""status": "Failed""#)
            ))
            .unwrap()
        ));
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(failed[0].starts_with("FAILED"), "{:?}", failed[0]);
        assert!(
            failed[0].contains("This model is no longer available to new users."),
            "marion's own diagnosis of the failure is the message: {:?}",
            failed[0]
        );
        assert!(
            failed[1].contains("the codex child failed"),
            "{:?}",
            failed[1]
        );

        // A tool result that is not a contract still takes the generic path rather than being
        // forced into a shape it does not have.
        let plain = shown(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"t","content":"{\"unrelated\":\"json\"}"}]}}"#,
        );
        assert!(plain[0].starts_with("ok"), "{:?}", plain[0]);
    }

    /// **A child whose work landed on a branch says so, with the command that takes it.** The
    /// work is no longer in any worktree, so without this line the watcher learns the child
    /// succeeded and has no idea where its change went.
    #[test]
    fn a_returned_contract_whose_work_landed_names_the_branch_and_the_merge() {
        let contract = r#"{
  "child": {"harness": "codex"},
  "completion": {
    "status": "Ok",
    "narrative": {"value": "added subtract", "truncated": false, "original_bytes": 14},
    "branch": "marion/01a0ca90",
    "commit": "52dff3a0123456789abcdef0123456789abcdef0"
  }
}"#;
        let lines = shown(&format!(
            r#"{{"type":"user","message":{{"content":[
                {{"type":"tool_result","tool_use_id":"toolu_1","content":{}}}]}}}}"#,
            serde_json::to_string(contract).unwrap()
        ));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("the codex child finished: added subtract"),
            "{lines:?}"
        );
        assert!(
            lines[1].ends_with(
                "changes on branch marion/01a0ca90 (52dff3a01234); merge with: git merge \
                 marion/01a0ca90"
            ),
            "{lines:?}"
        );
        let unlanded = shown(&format!(
            r#"{{"type":"user","message":{{"content":[
                {{"type":"tool_result","tool_use_id":"toolu_1","content":{}}}]}}}}"#,
            serde_json::to_string(&contract.replace("\"branch\"", "\"elsewhere\"")).unwrap()
        ));
        assert_eq!(unlanded.len(), 1, "no branch, no line: {unlanded:?}");
    }

    // --- the child's life, read out of the journal ------------------------------------------------

    fn child_lines(event: &ChildEvent) -> Vec<String> {
        let mut buf: Vec<u8> = Vec::new();
        render_child(event, &mut buf).expect("rendering a child event cannot fail");
        String::from_utf8(buf)
            .expect("the renderer writes UTF-8")
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    fn agent(n: &str) -> marion_core::contract::AgentId {
        marion_core::contract::AgentId(n.into())
    }

    /// A child starting is the event that used to be invisible for the whole of its life. It names
    /// the child's own agent type, which is the only thing that tells one child from another.
    #[test]
    fn a_child_starting_names_what_it_is_and_a_nameless_one_is_not_given_an_invented_name() {
        let lines = child_lines(&ChildEvent::Started {
            agent_id: agent("a-1"),
            agent_type: Some("codex-impl".into()),
            harness: Some(marion_core::harness::Harness::Codex),
            depth: Some(1),
            pid: Some(4242),
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("CHILD"), "{:?}", lines[0]);
        assert!(lines[0].contains("codex-impl started"), "{:?}", lines[0]);
        assert!(lines[0].contains("depth 1"), "{:?}", lines[0]);
        assert!(lines[0].contains("pid 4242"), "{:?}", lines[0]);

        // A node whose intent record this watch never read is real — its intent can predate the
        // cursor — and it is described, not named.
        let anonymous = child_lines(&ChildEvent::Started {
            agent_id: agent("a-2"),
            agent_type: None,
            harness: None,
            depth: None,
            pid: None,
        });
        assert!(anonymous[0].contains("a child started"), "{anonymous:?}");
    }

    /// The same truncate-success / keep-failure rule the rest of this renderer follows, and it
    /// earns its keep here: §6.7's exit description carries the child's own stderr.
    #[test]
    fn a_successful_childs_exit_is_brief_and_a_failed_ones_is_kept_whole() {
        let noisy = format!(
            "child exited with code 0; stderr: {}",
            "warning ".repeat(200)
        );
        let ok = child_lines(&ChildEvent::Exited {
            agent_id: agent("a-1"),
            agent_type: Some("codex-impl".into()),
            status: Some(ExitStatus::Ok),
            exit: Some(marion_core::contract::ProcessExit {
                code: Some(0),
                signal: None,
                description: noisy.clone(),
            }),
        });
        assert_eq!(ok.len(), 1, "{ok:?}");
        assert!(ok[0].starts_with("CHILD"), "{:?}", ok[0]);
        assert!(ok[0].contains("codex-impl ended: done"), "{:?}", ok[0]);
        assert!(
            ok[0].chars().count() < 300,
            "a successful child's warnings must not be dragged across the terminal: {} chars",
            ok[0].chars().count()
        );

        let failed = child_lines(&ChildEvent::Exited {
            agent_id: agent("a-1"),
            agent_type: Some("codex-impl".into()),
            status: Some(ExitStatus::TimedOut),
            exit: Some(marion_core::contract::ProcessExit {
                code: None,
                signal: Some(9),
                description: "the child's wall clock expired and its process group was killed"
                    .into(),
            }),
        });
        assert!(failed[0].starts_with("FAILED"), "{:?}", failed[0]);
        assert!(failed[0].contains("ended: timed out"), "{:?}", failed[0]);
        assert!(
            failed[0].contains("its process group was killed"),
            "a failure keeps every byte that explains it: {:?}",
            failed[0]
        );
    }

    /// A child that never started, a denial one layer down, and the viewer itself giving up — none
    /// of which is silent, and the last of which is the one that would otherwise look like a run
    /// in which nothing more happened.
    #[test]
    fn an_abort_a_childs_denial_and_the_watch_stopping_all_produce_a_line() {
        let aborted = child_lines(&ChildEvent::Aborted {
            agent_id: agent("a-1"),
            agent_type: Some("codex-impl".into()),
            reason: "the bridge never became ready".into(),
        });
        assert!(aborted[0].starts_with("FAILED"), "{aborted:?}");
        assert!(
            aborted[0].contains("never started: the bridge never became ready"),
            "{aborted:?}"
        );

        let denied = child_lines(&ChildEvent::Denied {
            agent_id: agent("a-1"),
            agent_type: Some("codex-impl".into()),
            tool: "mcp__marion__spawn".into(),
            reason: "the depth ceiling".into(),
        });
        assert!(denied[0].starts_with("PERMIT"), "{denied:?}");
        assert!(denied[0].contains("mcp__marion__spawn"), "{denied:?}");

        let stopped = child_lines(&ChildEvent::Stopped {
            reason: "line 4 is not a journal record; the run is unaffected".into(),
        });
        assert!(stopped[0].starts_with("view"), "{stopped:?}");
        assert!(stopped[0].contains("the run is unaffected"), "{stopped:?}");
    }

    /// **Two threads, one terminal, and not one torn render.**
    ///
    /// This is a correctness property, not a formatting one: the frame sink writes from the thread
    /// driving the run and the journal watch writes from the polling thread, both of them
    /// *multi-line* renders. Interleaved, a `CHILD` line lands in the middle of the root's prose
    /// and the first thing anyone suspects is marion, not a renderer. It only ever shows up under
    /// load, which is why it is asserted here rather than left to be noticed.
    ///
    /// The assertion is over **whole renders**: every three-line block must appear contiguously and
    /// in order. Counting lines would pass against fully interleaved output.
    #[test]
    fn two_threads_writing_at_once_cannot_split_each_others_lines() {
        let terminal = std::sync::Arc::new(Terminal::new(Vec::<u8>::new()));
        let rounds = 300;
        let threads: Vec<_> = ["root", "CHILD"]
            .into_iter()
            .map(|tag| {
                let terminal = std::sync::Arc::clone(&terminal);
                std::thread::spawn(move || {
                    for i in 0..rounds {
                        // Three lines per render, which is what makes splitting observable: a
                        // single-line writer would be atomic by accident and prove nothing.
                        terminal.show(&|w| {
                            let _ = say(w, tag, &format!("{tag} {i} first\nsecond\nthird"));
                        });
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("no writer panicked");
        }
        let out = match std::sync::Arc::try_unwrap(terminal) {
            Ok(t) => t.out.into_inner().unwrap_or_else(|e| e.into_inner()),
            Err(_) => panic!("a writer outlived the join"),
        };
        let lines: Vec<String> = String::from_utf8(out)
            .expect("the renderer writes UTF-8")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), rounds * 2 * 3, "every line was written");
        let mut blocks = 0;
        let mut i = 0;
        while i < lines.len() {
            let tag = lines[i].split_whitespace().next().unwrap_or_default();
            assert!(
                lines[i].contains("first"),
                "a render began mid-block, so two writers were interleaved. `Terminal::show` must \
                 hold the lock across a WHOLE render, not around each `write`: a per-write lock \
                 still lets the other thread's line land between two lines of this one. It bites \
                 only under load, and the symptom — a CHILD line spliced into the middle of the \
                 root's prose — reads as a protocol bug in marion rather than a formatting bug in \
                 the viewer, which is where anyone debugging it will start looking.\n{}\n{}\n{}",
                lines[i],
                lines.get(i + 1).cloned().unwrap_or_default(),
                lines.get(i + 2).cloned().unwrap_or_default()
            );
            for (n, expected) in ["second", "third"].iter().enumerate() {
                let line = lines
                    .get(i + 1 + n)
                    .unwrap_or_else(|| panic!("block starting at {i} is short"));
                assert!(
                    line.contains(expected) && line.starts_with("     "),
                    "the {tag} writer's render was split by the other thread: line {} of its \
                     block is {line:?}. `Terminal::show` must hold the lock across a WHOLE \
                     render, not around each `write` — a per-write lock still lets the other \
                     thread's line land between two lines of this one. It bites only under load, \
                     and the symptom — one writer's line spliced into the middle of the other's \
                     prose — reads as a protocol bug in marion rather than a formatting bug in \
                     the viewer, which is where anyone debugging it will start looking.",
                    n + 2
                );
            }
            blocks += 1;
            i += 3;
        }
        assert_eq!(blocks, rounds * 2);
    }

    /// **The view must not end on a lie.** A child that exits in the last moments before the root
    /// finishes — the bridge writes `Exited` just before returning the `spawn` result, and the run
    /// can end inside one poll interval of that — must still be announced. Reading the stop flag
    /// after the poll instead of before it would drop exactly those records, and the last thing a
    /// person saw would be a child that started and never finished.
    ///
    /// Deterministic rather than timed: the records land *during* the nap, from the test's own
    /// `nap`, after the stop flag is already set.
    /// **The tail sleeps until the journal is written or the run stops**, not on a timer: with an
    /// hour's safety poll, an append from another process and a raised stop flag each end the nap.
    #[test]
    fn the_journal_tail_naps_until_a_write_or_a_stop() {
        let dir = marion_testsupport::scratch("marion-bin-nap");
        let path = dir.join("journal.jsonl");
        std::fs::write(&path, b"").unwrap();
        let file = std::sync::Arc::new(std::sync::Mutex::new(Some(std::cell::RefCell::new(
            marion_supervisor::wake::Watch::new(&path),
        ))));
        let stop = std::sync::Arc::new(marion_supervisor::wake::Flag::new());
        let nap = |file: std::sync::Arc<std::sync::Mutex<Option<std::cell::RefCell<_>>>>,
                   stop: std::sync::Arc<marion_supervisor::wake::Flag>| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let cell = file.lock().unwrap().take().unwrap();
                nap_until_the_journal_changes(&cell, &stop, StdDuration::from_secs(3600));
                *file.lock().unwrap() = Some(cell);
                let _ = tx.send(());
            });
            rx
        };
        let woke = nap(std::sync::Arc::clone(&file), std::sync::Arc::clone(&stop));
        std::thread::sleep(StdDuration::from_millis(50));
        marion_testsupport::append(&path, b"{}\n");
        woke.recv_timeout(StdDuration::from_secs(10))
            .expect("an append did not end the nap");
        let woke = nap(std::sync::Arc::clone(&file), std::sync::Arc::clone(&stop));
        std::thread::sleep(StdDuration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
        woke.recv_timeout(StdDuration::from_secs(10))
            .expect("the stop did not end the nap");
    }

    #[test]
    fn a_child_that_exits_in_the_last_moments_of_a_run_is_still_announced() {
        use marion_core::contract::{AgentId, ProcessExit};
        use marion_core::journal::{
            Exited, JournalRecord, RecordKind, SpawnIntent, WriterId, encode,
        };
        use std::io::Write as _;

        let dir = marion_testsupport::scratch("marion-final-poll");
        let path = dir.join("journal.jsonl");
        let root = AgentId("root".into());
        let child = AgentId("child".into());
        let line = |seq: u64, kind: RecordKind| {
            let mut bytes = encode(&JournalRecord {
                seq,
                writer: WriterId("w".into()),
                ts: marion_core::encoding::SystemTime::from_unix_millis(1_785_625_628_619),
                mono_ns: seq,
                provenance: marion_core::ir::Provenance::marion(),
                src_seq: None,
                kind,
            })
            .expect("a record encodes");
            bytes.push(b'\n');
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&bytes).unwrap();
        };

        let mut watch = JournalWatch::at_end(&path, root.clone());
        let naps = std::cell::Cell::new(0);
        let mut seen: Vec<ChildEvent> = Vec::new();
        follow_journal(
            &mut watch,
            // Not stopped on the first pass; stopped on the second — so the third pass is the one
            // that must still happen.
            &|| naps.get() >= 2,
            &|| {
                let n = naps.get();
                naps.set(n + 1);
                match n {
                    0 => line(
                        1,
                        RecordKind::SpawnIntent(SpawnIntent {
                            review_of: None,
                            agent_id: child.clone(),
                            parent_id: Some(root.clone()),
                            agent_type: "codex-impl".into(),
                            harness: marion_core::harness::Harness::Codex,
                            depth: 1,
                            task_id: None,
                            timeout_secs: None,
                            verification: vec![],
                        }),
                    ),
                    // Written after the run is over, in the window between the stop and the last
                    // poll. This is the record the loop exists to still catch.
                    _ => line(
                        2,
                        RecordKind::Exited(Exited {
                            agent_id: child.clone(),
                            status: ExitStatus::Ok,
                            exit: ProcessExit {
                                code: Some(0),
                                signal: None,
                                description: "child exited with code 0".into(),
                            },
                        }),
                    ),
                }
            },
            &mut |e| seen.push(e.clone()),
        );
        assert!(
            matches!(seen.first(), Some(ChildEvent::Started { .. })),
            "{seen:?}"
        );
        assert!(
            matches!(
                seen.last(),
                Some(ChildEvent::Exited {
                    status: Some(ExitStatus::Ok),
                    ..
                })
            ),
            "a child that exited after the run was over went unannounced. The direction that \
             breaks this is hoisting the return ABOVE the poll — `if stopped() {{ return; }}` at \
             the top of the loop — which drops every record written between the last poll and the \
             stop signal. That window is not hypothetical: the bridge writes `Exited` and \
             `ContractPersisted` immediately before returning the `spawn` result, so a root that \
             finishes inside one poll interval of its child lands squarely in it. By then the \
             run's own summary is already on screen, so the view would end asserting a child was \
             still running when marion knew it had finished — a view that ends on a lie, which is \
             worse than one that ends late. (Reading the flag before rather than after the poll \
             within an iteration is NOT this bug: both orders still poll before returning, and \
             the mutation check confirmed both pass.) Saw: {seen:?}"
        );
    }

    /// A stdout line the node wrote that was not JSON is usually a crash or a warning. It is shown
    /// verbatim and marked as the node's, not marion's.
    #[test]
    fn a_line_that_was_not_json_is_shown_verbatim() {
        let mut buf: Vec<u8> = Vec::new();
        render_event(
            StreamEvent::Unparsed("thread 'main' panicked at src/x.rs:1:1"),
            &mut buf,
        )
        .unwrap();
        let shown = String::from_utf8(buf).unwrap();
        assert!(shown.starts_with("stdout"), "{shown:?}");
        assert!(
            shown.contains("thread 'main' panicked at src/x.rs:1:1"),
            "{shown:?}"
        );
    }

    /// A stream of frames through the live view, flushed at the end as the run's bookend flushes
    /// it, as the lines a person would see.
    fn shown_stream(frames: &[&str]) -> Vec<String> {
        let mut view = LiveView::default();
        let mut buf: Vec<u8> = Vec::new();
        for f in frames {
            let v: Value = serde_json::from_str(f).expect("the fixture frame parses");
            view.event(StreamEvent::Frame(&v), &mut buf).unwrap();
        }
        view.flush(&mut buf).unwrap();
        String::from_utf8(buf)
            .unwrap()
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    fn acp_update(update: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s1","update":{update}}}}}"#
        )
    }

    /// **An ACP root's stream reads as prose, not as a column of unknown frames.** Dispatched on
    /// the frame's JSON-RPC shape, never on the harness: message chunks coalesce into one line
    /// until the next non-chunk frame, a thought is summarised rather than shown, a tool call
    /// renders by its title and its update by its status, and `stopReason` is the run's `done`.
    #[test]
    fn acp_chunks_coalesce_and_a_tool_call_renders_by_its_title() {
        let chunk = |kind: &str, text: &str| {
            acp_update(&format!(
                r#"{{"sessionUpdate":"{kind}","content":{{"type":"text","text":"{text}"}}}}"#
            ))
        };
        let frames = [
            r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentInfo":{"name":"fake-acp","version":"0.1"}}}"#.to_string(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}"#.to_string(),
            chunk("agent_thought_chunk", "planning "),
            chunk("agent_thought_chunk", "it"),
            chunk("agent_message_chunk", "Delegating to "),
            chunk("agent_message_chunk", "a child."),
            acp_update(
                r#"{"sessionUpdate":"tool_call","toolCallId":"c1","title":"marion/spawn","status":"pending","rawInput":{"agent_type":"codex"}}"#,
            ),
            acp_update(
                r#"{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"failed","content":[{"type":"content","content":{"type":"text","text":"no such agent type"}}]}"#,
            ),
            chunk("agent_message_chunk", "Done."),
            r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}"#.to_string(),
        ];
        let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
        let lines = shown_stream(&frames);
        assert!(
            lines.iter().all(|l| !l.contains("no `type` field")),
            "every ACP frame has a renderer: {lines:#?}"
        );
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?}: {lines:#?}"))
        };
        assert!(lines[at("fake-acp")].starts_with("session"), "{lines:#?}");
        assert!(
            lines[at("(11 characters of reasoning)")].starts_with("think"),
            "two thought chunks are one summary: {lines:#?}"
        );
        let said = at("Delegating to a child.");
        assert!(lines[said].starts_with("root"), "{lines:#?}");
        let called = at("marion/spawn");
        assert!(
            said < called,
            "the text is flushed before the call: {lines:#?}"
        );
        assert!(
            lines[called].contains(r#"agent_type="codex""#),
            "{lines:#?}"
        );
        let failed = at("no such agent type");
        assert!(lines[failed].starts_with("FAILED"), "{lines:#?}");
        let last = at("Done.");
        let done = at("end_turn");
        assert!(lines[done].starts_with("done"), "{lines:#?}");
        assert!(
            last < done,
            "the trailing text is flushed by the stop: {lines:#?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("Delegating")).count(),
            1,
            "{lines:#?}"
        );
    }

    /// A finished `spawn` reads as the child's verdict, whether the agent mirrored the contract as
    /// text or the whole MCP result around it.
    #[test]
    fn a_finished_acp_spawn_renders_the_childs_verdict() {
        let contract = serde_json::json!({
            "child": {"harness": "acp"},
            "completion": {"status": "Ok", "narrative": {
                "value": "wrote the file", "truncated": false, "original_bytes": 14
            }},
        })
        .to_string();
        let wrapped =
            serde_json::json!({"content": [{"type": "text", "text": contract}]}).to_string();
        for text in [contract, wrapped] {
            let update = serde_json::json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": text}}],
            });
            let frame = acp_update(&update.to_string());
            let lines = shown_stream(&[&frame]);
            assert_eq!(lines.len(), 1, "{lines:#?}");
            assert!(lines[0].starts_with("CHILD"), "{lines:#?}");
            assert!(
                lines[0].contains("the acp child finished: wrote the file"),
                "{lines:#?}"
            );
        }
    }

    /// A chunk the stream ends on is not lost: the run's closing bookend flushes it.
    #[test]
    fn a_trailing_acp_chunk_is_flushed_at_the_end() {
        let frame = acp_update(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"last words"}}"#,
        );
        let lines = shown_stream(&[&frame]);
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(
            lines[0].starts_with("root") && lines[0].contains("last words"),
            "{lines:#?}"
        );
    }

    #[test]
    fn detach_is_a_valueless_run_flag() {
        let a = parse_args(&argv(&["run", "codex", "--prompt", "p", "--detach"])).unwrap();
        assert!(a.detach);
        let b = parse_args(&argv(&["run", "codex", "--prompt", "p"])).unwrap();
        assert!(!b.detach);
    }

    /// **Every home-screen effect is the command its box echoes**: each effect's `argv`, parsed
    /// back through this binary's own parsers, names the same node, text, type and model. An
    /// effect that drifted from its echo would teach the operator a command that does something
    /// else.
    #[test]
    fn every_home_effect_parses_back_to_itself() {
        use marion_core::contract::AgentId;
        use marion_supervisor::home::Effect;
        let verb = |e: &Effect| -> Vec<String> {
            let a = e.argv().expect("a command");
            assert_eq!(a[0], "marion", "{a:?}");
            a[1..].to_vec()
        };
        let id = AgentId("01a093dc-0e28-7854-bffe-f07bc4483c33".into());
        for (model, prompt, pane) in [
            (None, "add a limiter", false),
            (Some("opus"), "--looks like a flag, and \"quotes\"", true),
        ] {
            let run = Effect::Run {
                agent_type: "claude-orchestrator".into(),
                model: model.map(str::to_string),
                prompt: prompt.into(),
                pane,
            };
            let a = parse_args(&verb(&run)).expect("the run echo parses");
            assert_eq!(a.agent_type, "claude-orchestrator");
            assert_eq!(a.prompt, prompt);
            assert_eq!(a.model.as_deref(), model);
            assert_eq!(a.pane, pane);
            // Either way it returns once the root exists: home never watches a run itself.
            assert!(a.detach || a.pane);
        }
        let attach = parse_attach(&verb(&Effect::Attach(id.clone()))).unwrap();
        assert_eq!(attach.agent_id, id.0);
        let cancel = parse_attach(&verb(&Effect::Cancel(id.clone()))).unwrap();
        assert_eq!(cancel.agent_id, id.0);
        let resume = parse_resume(&verb(&Effect::Resume(id.clone()))).unwrap();
        assert_eq!(resume.agent_id, id.0);
        for text in ["use a deque", "-starts with a dash", "  two  spaces  "] {
            let steer = parse_steer(&verb(&Effect::Steer(id.clone(), text.into()))).unwrap();
            assert_eq!(steer.target, id.0);
            assert_eq!(steer.text, SteerText::Words(text.into()));
        }
        // Setup's key hand-offs run exactly the command they echo: the verb and one credential id,
        // which `marion login`/`logout` take as their only positional argument.
        for (e, v) in [
            (Effect::Login("openrouter:work".into()), "login"),
            (Effect::Logout("openrouter:work".into()), "logout"),
        ] {
            let a = verb(&e);
            assert_eq!(a, [v, "openrouter:work"]);
            assert!(marion_core::provider::CredentialId::parse(&a[1]).is_some());
        }
        // The verbs the echoes name are the verbs this binary dispatches.
        for v in [
            "attach", "cancel", "resume", "steer", "login", "logout", "run",
        ] {
            assert!(
                cli::verb(v).is_some(),
                "`marion {v}` is echoed and not dispatched"
            );
        }
    }

    #[test]
    fn run_takes_the_agent_type_positionally_and_the_prompt_as_a_flag() {
        let a = parse_args(&argv(&["run", "claude", "--prompt", "delegate it"])).unwrap();
        assert_eq!(a.agent_type, "claude");
        assert_eq!(a.prompt, "delegate it");
        assert!(a.timeout_secs.is_none());
        assert!(a.profile.is_none(), "no profile unless one is named");
    }

    #[test]
    fn run_takes_the_profile_the_operator_names() {
        let a = parse_args(&argv(&[
            "run",
            "claude",
            "--prompt",
            "x",
            "--profile",
            "work",
        ]))
        .unwrap();
        assert_eq!(a.profile.as_deref(), Some("work"));
        assert!(parse_args(&argv(&["run", "claude", "--prompt", "x", "--profile"])).is_err());
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        // A silently-ignored --state-dir typo writes the run's state somewhere else entirely and
        // leaves the operator hunting for a contract that is not where they looked.
        assert!(
            parse_args(&argv(&[
                "run",
                "claude",
                "--prompt",
                "p",
                "--stat-dir",
                "/x"
            ]))
            .is_err()
        );
    }

    #[test]
    fn a_prompt_is_required_because_a_root_with_no_turn_does_nothing() {
        assert!(parse_args(&argv(&["run", "claude"])).is_err());
        assert!(parse_args(&argv(&["run", "--prompt", "p"])).is_err());
    }

    /// The canned provider is a **fixture**, so reaching it is the thing that must be typed. A
    /// person running marion means their own logged-in harness, and a default that silently aimed
    /// them at a fake server on 127.0.0.1 answered a question nobody asked.
    #[test]
    fn canned_is_a_valueless_flag_and_real_auth_is_what_a_bare_run_gets() {
        assert!(
            !parse_args(&argv(&["run", "claude", "--prompt", "p"]))
                .unwrap()
                .backend
                .canned
        );
        let a = parse_args(&argv(&["run", "claude", "--prompt", "p", "--canned"])).unwrap();
        assert!(a.backend.canned);
        assert!(a.backend.base_url.is_none());
        // It takes no value, so a following flag is still parsed as a flag rather than eaten.
        let a = parse_args(&argv(&["run", "claude", "--canned", "--prompt", "p"])).unwrap();
        assert!(a.backend.canned && a.prompt == "p");
    }

    /// `--live` used to select real auth. Real auth is now the default, so the flag says nothing
    /// the run does not already do — but every script and note carrying it must keep working, so
    /// it is accepted and inert rather than a refusal.
    #[test]
    fn live_is_still_accepted_and_now_means_exactly_nothing() {
        let a = parse_args(&argv(&["run", "claude", "--prompt", "p", "--live"])).unwrap();
        assert!(
            !a.backend.canned,
            "--live is the default, not the canned provider"
        );
        assert_eq!(a.prompt, "p");
        // Still valueless: a following flag is parsed, not eaten.
        let a = parse_args(&argv(&["run", "claude", "--live", "--prompt", "p"])).unwrap();
        assert_eq!(a.prompt, "p");
        // And it composes with the flag that replaced it rather than fighting it.
        assert!(
            parse_args(&argv(&[
                "run", "claude", "--prompt", "p", "--live", "--canned"
            ]))
            .unwrap()
            .backend
            .canned
        );
    }

    /// **The safety gate.** Without `--canned` the node presents the operator's own credential; a
    /// loopback endpoint is marion's canned provider or some other local process. The combination
    /// sends a real credential to a fake server, and it is the one flag conflict whose cost is the
    /// operator's account rather than the run — so it is an error, not a precedence rule. Inverting
    /// the default moved this gate onto the *common* path, so it matters more than it used to.
    #[test]
    fn a_loopback_base_url_without_canned_is_refused_and_the_refusal_says_why() {
        for u in [
            "http://127.0.0.1:8099/v1",
            "http://localhost:8099/v1",
            "http://[::1]:8099/v1",
            "https://127.0.0.2/v1",
            "http://127.0.0.1",
        ] {
            let e = resolve_base_url(false, Some(u.into()), None)
                .expect_err("{u} is loopback and real auth must refuse it");
            assert!(e.contains(u), "the refusal must quote the URL: {e}");
            assert!(
                e.contains("real credential") && e.contains("fake server"),
                "it must name why, not merely refuse: {e}"
            );
            assert!(
                e.contains("--canned"),
                "it must name the flag that makes the run legal: {e}"
            );
            // …and it is precisely a *conflict*: the same URL under --canned is the normal case.
            assert_eq!(
                resolve_base_url(true, Some(u.into()), None),
                Ok(Some(u.into()))
            );
        }
    }

    /// **A gateway under real auth is refused because marion does not implement it.**
    ///
    /// This assertion used to be its own inverse: the endpoint was returned, on the reasoning that
    /// a proxy is a legitimate thing to point a real credential at. That reasoning is still sound
    /// as a *feature* and wrong as a *description* — no adapter ever passed the endpoint on. Under
    /// `Auth::Inherited` claude compiles `(None, None)`, gemini gates its `GOOGLE_GEMINI_BASE_URL`
    /// push on `Canned`, and codex and opencode return from `config_files` before reading it. So
    /// the operator got a run that reached the vendor directly and looked like it had honoured the
    /// gateway, which is the §12 accept-and-ignore shape with marion on the wrong side of it.
    ///
    /// The refusal is the reversible direction (`spawn::SpawnError`'s `isolation: "remote"` is
    /// the precedent): honouring can land later, but an operator taught that
    /// marion silently reaches the vendor cannot be un-taught.
    #[test]
    fn a_gateway_base_url_under_real_auth_is_refused_as_unimplemented_not_silently_dropped() {
        let u = "https://gateway.example.com/v1";
        let e = resolve_base_url(false, Some(u.into()), None)
            .expect_err("marion drops this endpoint, so accepting it would be a promise it breaks");
        assert!(e.contains(u), "the refusal must quote the URL: {e}");
        assert!(
            e.contains("not implemented"),
            "it must say marion *will not*, not merely that the flag is wrong here — the operator \
             needs to tell an unimplemented capability from a mistyped one: {e}"
        );
        assert!(
            e.contains("--canned"),
            "it must name the flag that makes the run legal: {e}"
        );
        assert!(
            e.contains("reached the vendor directly"),
            "it must say what would otherwise have happened, which is the part the operator cannot \
             observe: {e}"
        );
    }

    /// **The refusal must not swallow the loopback diagnosis.** Both arms now refuse and both name
    /// `--canned`, so the only thing keeping them apart is the reason — and the reasons are not
    /// interchangeable: one is "marion cannot do this yet", the other is "this would send your
    /// credential to a fake server". An operator who reads the wrong one draws the wrong lesson.
    #[test]
    fn the_two_refusals_stay_distinguishable_by_the_reason_they_give() {
        let loopback = resolve_base_url(false, Some("http://127.0.0.1:8099/v1".into()), None)
            .expect_err("loopback is refused");
        let gateway = resolve_base_url(false, Some("https://gateway.example.com/v1".into()), None)
            .expect_err("a gateway is refused");
        assert!(
            loopback.contains("fake server") && !loopback.contains("not implemented"),
            "the loopback refusal is a safety diagnosis, not a capability one: {loopback}"
        );
        assert!(
            gateway.contains("not implemented") && !gateway.contains("fake server"),
            "the gateway refusal is a capability diagnosis, not a safety one: {gateway}"
        );
    }

    /// **The half a careless refusal breaks.** `--canned` plus `--base-url` is how every integration
    /// test in this workspace launches — `m1_hop`, `cross_product`, `journal_wiring` and
    /// `launch_only_root` all pass the pair — so widening the gate to "any `--base-url` is refused"
    /// would take the whole suite down with it. Both a loopback fixture endpoint and a non-loopback
    /// one stay accepted, since `--canned` says where the run is pointed and marion obeys it there.
    #[test]
    fn canned_still_accepts_an_explicit_endpoint_which_is_how_the_suite_launches() {
        assert_eq!(
            resolve_base_url(true, Some(CANNED_BASE_URL.into()), None),
            Ok(Some(CANNED_BASE_URL.into())),
            "the exact pair every integration test passes"
        );
        assert_eq!(
            resolve_base_url(true, Some("http://192.0.2.7:8099/v1".into()), None),
            Ok(Some("http://192.0.2.7:8099/v1".into())),
            "a canned provider on another host is still a canned provider"
        );
    }

    /// The default means *marion names no endpoint*, so each harness resolves the vendor it is
    /// already logged in to. `$MARION_BASE_URL` is ignored rather than inherited — it names marion's
    /// canned server, and inheriting it silently is exactly the accident the gate above refuses
    /// loudly.
    #[test]
    fn the_default_is_no_base_url_at_all_and_ignores_the_canned_one_in_the_environment() {
        assert_eq!(resolve_base_url(false, None, None), Ok(None));
        assert_eq!(
            resolve_base_url(false, None, Some(CANNED_BASE_URL.into())),
            Ok(None),
            "an inherited canned endpoint under real auth is the very mistake being prevented"
        );
    }

    /// And canned mode's precedence is untouched: the flag, then the environment, then the default.
    #[test]
    fn under_canned_the_base_url_falls_back_from_the_flag_to_the_environment_to_the_loopback_default()
     {
        assert_eq!(
            resolve_base_url(true, Some("http://x/v1".into()), Some("http://y/v1".into())),
            Ok(Some("http://x/v1".into()))
        );
        assert_eq!(
            resolve_base_url(true, None, Some("http://y/v1".into())),
            Ok(Some("http://y/v1".into()))
        );
        assert_eq!(
            resolve_base_url(true, None, None),
            Ok(Some(CANNED_BASE_URL.into())),
            "--canned means the loopback endpoint without having to also say where"
        );
    }

    #[test]
    fn loopback_detection_reads_the_host_and_not_the_rest_of_the_url() {
        for u in [
            "http://127.0.0.1:8099/v1",
            "http://127.1.2.3/v1",
            "http://LOCALHOST:1/v1",
            "http://user:pw@localhost:8099/v1",
            "http://[::1]:8099/v1",
            "localhost:8099",
        ] {
            assert!(is_loopback_url(u), "{u} is loopback");
        }
        for u in [
            "https://api.anthropic.com",
            "https://gateway.example.com/v1",
            // The host is what decides, not a path or a query that merely mentions it.
            "https://example.com/127.0.0.1",
            "https://example.com/v1?to=localhost",
            // Not in 127/8, and not a name that resolves there by convention.
            "http://12.7.0.1/v1",
            "http://notlocalhost.example.com",
        ] {
            assert!(!is_loopback_url(u), "{u} is not loopback");
        }
    }

    /// `--repo`'s default. Running marion from `crates/marion-supervisor/src` must scope the node
    /// to the checkout, not to `src`, which is what the previous plain-cwd default did.
    #[test]
    fn the_repo_default_walks_up_to_the_enclosing_git_root() {
        let root = scratch("cli-gitroot");
        let deep = root.join("crates/x/src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(git_root(&deep), Some(root.to_path_buf()));
        assert_eq!(
            default_repo(&deep),
            root.to_path_buf(),
            "from any depth, the same root"
        );
        assert_eq!(
            git_root(&root),
            Some(root.to_path_buf()),
            "the root finds itself"
        );

        // The nearest `.git` wins, so a nested checkout is not swallowed by its container.
        let inner = root.join("crates/x");
        std::fs::write(inner.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(
            git_root(&deep),
            Some(inner),
            "a `.git` *file* is a linked worktree or a submodule, and is quite as much a root"
        );
    }

    #[test]
    fn with_no_git_anywhere_above_the_repo_default_is_the_working_directory_itself() {
        // Two bindings, not `scratch("cli-nogit").join("a/b")`. That one-liner is what this test
        // used to read, and it was safe only because the old local helper returned a bare
        // `PathBuf`: against a guard it drops at the end of the statement and deletes the
        // directory out from under the assertions below.
        let root = scratch("cli-nogit");
        let dir = root.join("a/b");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(git_root(&dir), None);
        assert_eq!(default_repo(&dir), dir, "a fallback, never a failure");
    }

    /// **`--help` names the headline commands and the real tool list.** The native `marion
    /// <harness>` lanes come from the facade registry, so a lane switched on is listed the same
    /// day; the MCP tools are read off the list `marion mcp` actually declares — without `report`,
    /// which only a child marion started can use.
    #[test]
    fn the_help_lists_the_native_commands_and_the_declared_tools() {
        let top = cli::top_help();
        for command in production_native_facades().enabled_native_commands() {
            assert!(
                top.contains(command),
                "`marion {command}` is missing:\n{top}"
            );
        }
        let declared = mcp_tool_names();
        assert_eq!(declared, ["spawn", "wait", "status", "list", "steer"]);
        let mcp = (cli::verb("mcp").unwrap().help)();
        assert!(mcp.contains(&declared.join(", ")), "{mcp}");
    }

    /// `run --help` is the only place its flags are stated to a person, so it has to track them. A
    /// stale line here is the same bug as a stale default.
    #[test]
    fn the_run_help_describes_the_flags_that_exist() {
        let u = (cli::verb("run").unwrap().help)();
        for flag in [
            "--prompt",
            "--model",
            "--timeout",
            "--profile",
            "--pane",
            "--detach",
            "--no-change-record",
            "--canned",
            "--base-url",
            "--repo",
            "--state-dir",
        ] {
            assert!(u.contains(flag), "{flag} is not documented:\n{u}");
        }
        // Inert, and kept only for old scripts: documenting it would be advertising nothing.
        assert!(!u.contains("--live"), "{u}");
        // `--base-url` must not read as a way to reach a gateway under real auth, which marion
        // refuses as unimplemented.
        assert!(u.contains("(only with --canned)"), "{u}");
        for name in marion_core::agent_type::builtin_names() {
            assert!(u.contains(name), "{name} must be listed");
        }
        // The escape hatch on the grant gate, and what it costs, since it is a trade and not a
        // bypass: the refusal names it, and so must `--help`.
        assert!(u.contains("no file tools"), "{u}");
    }

    /// **The gate's escape hatch parses, and it is valueless.**
    ///
    /// Valueless for `--canned`'s reason: declining the audit a grant is conditional on is a
    /// decision, and it should read as one at the call site rather than as a setting. The default
    /// is asserted beside it, because a flag that defaulted the other way would turn the gate off
    /// for every run without anyone typing anything.
    #[test]
    fn the_change_record_is_taken_unless_the_operator_says_otherwise() {
        let a = parse_args(&argv(&["run", "claude-impl", "--prompt", "p"])).unwrap();
        assert!(!a.no_change_record, "the record is taken by default");
        let b = parse_args(&argv(&[
            "run",
            "claude-impl",
            "--prompt",
            "p",
            "--no-change-record",
        ]))
        .unwrap();
        assert!(b.no_change_record);
        // And it is still an unknown-flag refusal away from being a typo that silently disarmed
        // the gate.
        assert!(
            parse_args(&argv(&[
                "run",
                "claude",
                "--prompt",
                "p",
                "--no-change-recrd"
            ]))
            .is_err()
        );
    }

    #[test]
    fn the_blocked_bound_falls_back_from_the_flag_to_the_agent_type_to_900() {
        assert_eq!(blocked_bound_secs(Some(60), 900), 60);
        assert_eq!(blocked_bound_secs(None, 900), 900);
        assert_eq!(
            blocked_bound_secs(None, 0),
            marion_core::agent_type::DEFAULT_TIMEOUT_SECS,
            "a zero would expire every Blocked episode instantly"
        );
        assert_eq!(
            blocked_bound_secs(Some(0), 900),
            marion_core::agent_type::DEFAULT_TIMEOUT_SECS
        );
    }

    fn ids(names: &[&str]) -> Vec<marion_core::contract::AgentId> {
        names
            .iter()
            .map(|n| marion_core::contract::AgentId((*n).into()))
            .collect()
    }

    fn guidance() -> marion_core::proto::DetachGuidance {
        marion_core::proto::DetachGuidance {
            reattach: "Reconnect to /s/p/supervisor.sock and call tree/subscribe.".into(),
            stop_fleet: "Reconnect to /s/p/supervisor.sock and call session/quit with KillTree."
                .into(),
        }
    }

    /// **NC — §7.3.2's "never silently detach" is five facts, and a receipt is not one of them.**
    ///
    /// *"They MUST tell the operator both how to re-attach and how to stop the fleet without one.
    /// Detaching into silence is worse than killing, because the operator does not know they now
    /// own something."* The version this replaces printed `Resident(NonTerminalNode)` and stopped:
    /// the operator was told a supervisor existed and not one thing about what it held, what could
    /// be denied unattended while nobody watched, or how to reach or end it. Every one of those
    /// was already in the response and was discarded on the way to the terminal.
    #[test]
    fn a_detach_reports_every_fact_section_7_3_2_requires_before_leaving_a_fleet_running() {
        let (supervisor, report) = detach_report(&marion_core::proto::QuitOutcome::Detached {
            detached: ids(&["root", "child"]),
            gate_exposed: ids(&["child"]),
            guidance: guidance(),
            supervisor: marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::NonTerminalNode,
            ),
        })
        .expect("a detach renders");
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::NonTerminalNode
            ),
            "the caller's wait decision is the rendered fact, never a second match on the outcome"
        );
        assert!(report.contains("an agent is still running"), "{report}");
        assert!(!report.contains("NonTerminalNode"), "{report}");
        assert!(
            report.contains("root, child"),
            "the nodes left running are named: {report}"
        );
        assert!(
            report.contains("nobody can approve a permission for child"),
            "§7.3.2's stated cost of (b) names the exposed nodes: {report}"
        );
        assert!(
            report.contains("refused"),
            "and says what that costs: {report}"
        );
        assert!(
            !report.contains('§'),
            "a person meets no design-doc section numbers: {report}"
        );
        assert!(
            report.contains("`marion ls`") && report.contains("`marion attach root`"),
            "how to get back: {report}"
        );
        assert!(
            report.contains("`marion cancel <id>`") && !report.contains("supervisor.sock"),
            "and how to stop it, as a command rather than a protocol call: {report}"
        );
        assert_eq!(
            report.lines().count(),
            3,
            "disposition, the fleet, and the gate warning — nothing more: {report}"
        );
    }

    /// **A detach with nothing at a gate is one line after the disposition.** §7.3.2's facts are
    /// all still there — what is running, how to get back, how to stop it — on one line; the
    /// permission-gate warning is a second line only for the fleet that earns it: a node that can
    /// be denied unattended while nobody is watching.
    #[test]
    fn a_detach_with_nothing_at_a_gate_reports_in_one_line() {
        let (_, report) = detach_report(&marion_core::proto::QuitOutcome::Detached {
            detached: ids(&["root"]),
            gate_exposed: Vec::new(),
            guidance: guidance(),
            supervisor: marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::NonTerminalNode,
            ),
        })
        .expect("a detach renders");
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "disposition, then one line for the fleet: {report}"
        );
        let fleet = lines[1];
        assert!(
            fleet.contains("still running in the background: root"),
            "{fleet}"
        );
        assert!(fleet.contains("`marion ls`"), "how to get back: {fleet}");
        assert!(
            fleet.contains("`marion attach root`"),
            "how to get back: {fleet}"
        );
        assert!(
            fleet.contains("`marion cancel <id>`"),
            "how to stop it without getting back: {fleet}"
        );
        assert!(
            !report.contains("nobody can approve"),
            "nothing is exposed: {report}"
        );
    }

    /// The other half: a run that left nothing running says so plainly rather than printing
    /// re-attach instructions for an empty fleet, and disposition (c)'s reaped set — which §7.3.2
    /// requires be reported *as resumable* — survives the render too.
    #[test]
    fn a_detach_that_left_nothing_running_says_so_and_a_reap_names_what_is_resumable() {
        let (supervisor, report) = detach_report(&marion_core::proto::QuitOutcome::Detached {
            detached: Vec::new(),
            gate_exposed: Vec::new(),
            guidance: guidance(),
            supervisor: marion_core::proto::SupervisorDisposition::Exiting,
        })
        .expect("a detach renders");
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Exiting
        );
        assert!(report.contains("no node was left running"), "{report}");
        assert!(
            !report.contains("to re-attach"),
            "there is nothing to re-attach to: {report}"
        );

        let (_, reaped) = detach_report(&marion_core::proto::QuitOutcome::ReapedAndDetached {
            reaped: ids(&["idle"]),
            detached: ids(&["busy"]),
            gate_exposed: Vec::new(),
            guidance: guidance(),
            supervisor: marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::NonTerminalNode,
            ),
        })
        .expect("a reap renders");
        assert!(reaped.contains("reaped and resumable: idle"), "{reaped}");
        assert!(
            reaped.contains("still running in the background: busy"),
            "{reaped}"
        );
        for r in [&report, &reaped] {
            assert!(!r.contains('§'), "no design-doc section numbers: {r}");
        }
        let (_, resident) = detach_report(&marion_core::proto::QuitOutcome::Detached {
            detached: ids(&["busy"]),
            gate_exposed: Vec::new(),
            guidance: guidance(),
            supervisor: marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::NonTerminalNode,
            ),
        })
        .expect("a detach renders");
        assert!(
            !resident.contains('§'),
            "no design-doc section numbers: {resident}"
        );
    }

    /// **A permission ask a run cannot answer is said in words.** The line used to cite a design
    /// section to a person who has never read it.
    #[test]
    fn an_unanswerable_permission_ask_names_no_spec_section() {
        let frame = serde_json::json!({
            "type": "control_request",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash"},
        });
        let mut out = Vec::new();
        render_control_request(&frame, &mut out).expect("renders");
        let line = String::from_utf8(out).expect("utf-8");
        assert!(line.contains("Bash"), "{line}");
        assert!(line.contains("denied"), "says what happens: {line}");
        assert!(!line.contains('§'), "{line}");
    }

    /// **NC — the idle grace `marion run` asks for is §5.7's, not one this client invented.**
    ///
    /// It used to pass zero, reasoning that a finished run knows no successor is coming *from it*.
    /// The grace is not that client's property: whichever client happens to **start** a supervisor
    /// fixes it for every later one, so a TUI attaching to a run's supervisor inherited a zero it
    /// never chose and identical behaviour depended on a startup race. §5.7 picked 300 s for
    /// exactly the case that produced — *"an operator closing one window to open another"*.
    ///
    /// A zero is also no longer merely a policy choice here: `handler.rs`'s exit predicate is now
    /// §5.7's two clauses alone, so a supervisor launched with no grace could satisfy them and
    /// leave before the client that started it had finished connecting.
    #[test]
    fn marion_run_asks_for_section_5_7s_own_grace_rather_than_choosing_one() {
        assert_eq!(
            RUN_IDLE_GRACE,
            marion_supervisor::serve::DEFAULT_IDLE_GRACE,
            "the supervisor's default is the only number a client may ask for"
        );
        assert_eq!(
            RUN_IDLE_GRACE,
            StdDuration::from_secs(300),
            "§5.7's proposed, explicitly unmeasured grace"
        );
    }

    /// **One rule for what a client does once its root exists, shared by `run` and `resume`.**
    ///
    /// A headless root is watched to its exit — that is the whole of how the client sees the
    /// run — and a paned root is left to `marion attach`, because this client has no terminal
    /// and would otherwise hold the node's one write lease. `marion resume` used to hand a
    /// headless root to the attach path instead, which refused it (*"no display plane"*) and
    /// exited 1 while the node kept running.
    #[test]
    fn a_headless_root_is_watched_and_a_paned_root_is_left_to_attach() {
        assert_eq!(root_follow(false), RootFollow::Watch);
        assert_eq!(root_follow(true), RootFollow::Started);
    }

    /// The shape a resume follows is the shape the journal recorded for the node — the same
    /// record the supervisor relaunched it from — and the bound is the node's own, as `run`
    /// resolves it: the recorded one, else the type's.
    #[test]
    fn a_resumed_nodes_shape_and_bound_are_read_off_its_journal() {
        use marion_core::contract::AgentId;
        use marion_core::journal::{RecordKind, SessionObserved, SpawnIntent};
        use marion_supervisor::journal::append_at;

        let dir = scratch("marion-resumed-shape");
        let path = dir.join("journal.jsonl");
        let id = AgentId("root".into());
        let intent = |timeout_secs: Option<u64>| {
            RecordKind::SpawnIntent(SpawnIntent {
                review_of: None,
                agent_id: id.clone(),
                parent_id: None,
                agent_type: "codex-impl".into(),
                harness: marion_core::harness::Harness::Codex,
                depth: 0,
                task_id: None,
                timeout_secs,
                verification: vec![],
            })
        };
        let session = |pane: bool| {
            RecordKind::SessionObserved(SessionObserved {
                agent_id: id.clone(),
                harness: marion_core::harness::Harness::Codex,
                session_id: "thread-1".into(),
                pane,
                workspace: None,
                profile: None,
            })
        };

        // Nothing on disk: headless, the type's own bound — never a refusal, the node exists.
        let absent = resumed_shape(&path, &id);
        assert!(!absent.pane);
        assert_eq!(
            absent.bound,
            builtin("codex-impl").unwrap().timeout.0,
            "with no intent the bound is the default, not zero"
        );

        append_at(&path, intent(Some(300))).unwrap();
        append_at(&path, session(false)).unwrap();
        let headless = resumed_shape(&path, &id);
        assert!(!headless.pane, "the session recorded a headless launch");
        assert_eq!(
            headless.bound,
            StdDuration::from_secs(300),
            "the recorded bound wins"
        );
        assert_eq!(headless.agent_type, "codex-impl");

        append_at(&path, session(true)).unwrap();
        assert!(
            resumed_shape(&path, &id).pane,
            "the last session record decides the shape, as replay folds it"
        );
    }
}
