//! **`marion search`**: which run did X, in this project's history.
//!
//! The scan is [`marion_supervisor::search::find`], the function the TUI calls too; this module
//! reads the words, adds what only a serving supervisor knows (runs still live, and the live state
//! of a run it found), and prints. Every row is one physical line: recorded text goes through
//! `out!`, which spells a control, a bidi override or a zero-width character as its escape, and
//! `--json` keeps everything an agent or a person wrote under `untrusted`.

use marion_supervisor::{err, out};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use marion_core::node::NodeState;
use marion_core::proto::model::NodeSummary;
use marion_core::search::{Query, State};
use marion_supervisor::search::digest::{PROMPT_CAP, Run};
use marion_supervisor::search::open::{Kind, read_agent_file};
use marion_supervisor::search::{self, Found, Hit, Matcher, Options, Project};
use marion_supervisor::socket::{self, SocketPaths};
use marion_supervisor::tree;

use super::cli::{self, Exit, Place, Word, Words};

/// Runs shown when `--limit` is not given.
pub const DEFAULT_LIMIT: usize = 20;

/// How long a failed dial waits for the supervisor's lock to go. A supervisor that answered a
/// run's `session/quit` unlinks its socket before it lets its lock go, so a search in that gap
/// finds the lock held and nobody to dial.
const LEAVING: Duration = Duration::from_secs(2);

/// What `marion search` was asked.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SearchArgs {
    pub words: Vec<String>,
    pub json: bool,
    pub limit: Option<usize>,
    pub reindex: bool,
    /// Every project under the state dir, not only this one.
    pub all: bool,
    pub place: Place,
}

pub fn parse(argv: &[String]) -> Result<SearchArgs, Exit> {
    let mut words = Words::after_verb(argv);
    let mut args = SearchArgs::default();
    while let Some(word) = words.next()? {
        match word {
            Word::Flag("--json", v) => args.json = cli::switch("--json", v)?,
            Word::Flag("--reindex", v) => args.reindex = cli::switch("--reindex", v)?,
            Word::Flag("--all", v) => args.all = cli::switch("--all", v)?,
            Word::Flag("--limit", v) => {
                let n = words.value("--limit", v)?;
                args.limit = Some(
                    n.parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| Exit::Usage(format!("--limit {n} is not a count")))?,
                );
            }
            Word::Flag(f, v) if args.place.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(w) => args.words.push(w.to_string()),
        }
    }
    Ok(args)
}

pub fn main(argv: &[String]) -> Result<ExitCode, Exit> {
    let args = parse(argv)?;
    let now = now_ms();
    let words: Vec<&str> = args.words.iter().map(String::as_str).collect();
    let query = Query::parse_args(&words, now).map_err(|e| Exit::Usage(e.to_string()))?;
    let Some((repo, state)) = super::resolve_project(&args.place) else {
        return Ok(ExitCode::FAILURE);
    };
    Ok(run(&args, &query, &repo, &state, now))
}

fn run(args: &SearchArgs, query: &Query, repo: &Path, state: &Path, now: u64) -> ExitCode {
    let started = Instant::now();
    let key = socket::project_root(repo);
    // **The tripwire, before anything is read**: a serving supervisor is dialled as the operator,
    // and the hello refuses a process inside one of its nodes. Nothing is dialled where nobody
    // serves, and no supervisor is started.
    let live = match live_nodes(repo, state, &key) {
        Ok(nodes) => nodes,
        Err(why) => {
            err!("marion search: {why}");
            return ExitCode::FAILURE;
        }
    };
    let current = Project::current(state, &key);
    // `project:` names projects, so it searches every one.
    let all = args.all || !query.project.is_empty();
    let mut projects = if all {
        let mut every = search::every(state);
        for p in every.iter_mut().filter(|p| p.hash() == current.hash()) {
            p.root = current.root.clone();
        }
        every
    } else {
        vec![current.clone()]
    };
    if all && let Err(why) = hello_every_live(state, &projects, &current) {
        err!("marion search: {why}");
        return ExitCode::FAILURE;
    }
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    let opts = Options {
        limit,
        reindex: args.reindex,
    };
    let mut outcome = search::find(&mut projects, query, opts);
    for (i, why) in &outcome.failed {
        err!("marion search: {}: {why}", projects[*i].name());
    }
    let matcher = Matcher::new(query);
    let here = projects
        .iter()
        .position(|p| p.hash() == current.hash())
        .filter(|i| projects[*i].wanted_by(query));
    let mut rows: Vec<Row> = match here {
        Some(i) => live_rows(&projects[i], i, &live, &matcher, &outcome.found),
        None => Vec::new(),
    };
    rows.extend(outcome.found.drain(..).map(|f| {
        // Live state is this project's supervisor's to say; other projects' runs read as their
        // digests recorded them.
        let live = live
            .iter()
            .filter(|_| Some(f.project) == here)
            .find(|n| n.agent_id == f.hit.run.agent_id)
            .and_then(|n| live_state(n.state));
        Row { found: f, live }
    }));
    rows.truncate(limit);
    print_rows(&rows, &projects, all, args.json, now);
    let ms = started.elapsed().as_millis();
    let runs = if rows.len() == 1 { "run" } else { "runs" };
    if rows.is_empty() {
        let scope = if all {
            "every project".to_string()
        } else {
            current.name()
        };
        err!(
            "no runs match {} in {scope} · searched {} in {ms} ms",
            shown_query(&args.words),
            outcome.searched
        );
    } else {
        err!(
            "{} {runs} · searched {} in {ms} ms",
            rows.len(),
            outcome.searched
        );
    }
    if let Some(words) = outcome.skipped.words() {
        err!("{words}");
    }
    if rows.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// One row: a run found, and its live state where a supervisor says it runs again.
struct Row {
    found: Found,
    live: Option<State>,
}

/// The tree of the supervisor serving `key`, when one is; none where nobody serves.
fn live_nodes(repo: &Path, state: &Path, key: &Path) -> Result<Vec<NodeSummary>, String> {
    let paths = socket::socket_paths(state, key, socket::own_uid());
    if socket::nobody_is_serving(&paths) {
        return Ok(Vec::new());
    }
    tree::snapshot(repo, state).or_else(|why| {
        if left(&paths) {
            Ok(Vec::new())
        } else {
            Err(format!("search stopped: {why}"))
        }
    })
}

/// Whether the supervisor at `paths` lets its lock go within [`LEAVING`]: it was on its way out,
/// so nothing of it is live. One still serving keeps the lock, and the failed dial stands — a
/// node that unlinks the socket under a live supervisor is still refused.
fn left(paths: &SocketPaths) -> bool {
    let deadline = Instant::now() + LEAVING;
    loop {
        if socket::nobody_is_serving(paths) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// **The tripwire for `--all`**: every other project whose root is known and whose supervisor is
/// serving is dialled with an operator hello, which refuses a process inside one of its nodes. A
/// project nobody serves is never dialled, and none is started. Not a boundary: the file mode on
/// `<state>/private/` is. Two known gaps until `socket::discover` lists live projects itself:
/// learning the roots ([`search::every`]) reads each project's `project.json` or digest header
/// first, and a live project whose root neither records is not dialled.
fn hello_every_live(state: &Path, projects: &[Project], current: &Project) -> Result<(), String> {
    for p in projects.iter().filter(|p| p.hash() != current.hash()) {
        let Some(root) = &p.root else { continue };
        let paths = socket::socket_paths(state, root, socket::own_uid());
        if socket::nobody_is_serving(&paths) {
            continue;
        }
        if let Err(why) = marion_supervisor::client_auth::dial(&paths)
            && !left(&paths)
        {
            return Err(format!("search stopped: {}: {why}", p.name()));
        }
    }
    Ok(())
}

/// A live node's state as `state:` spells it; `None` once it has ended.
fn live_state(state: NodeState) -> Option<State> {
    match state {
        NodeState::Exited(_) => None,
        NodeState::Idle | NodeState::Blocked(_) => Some(State::Waiting),
        NodeState::Spawning | NodeState::Ready | NodeState::Running => Some(State::Running),
    }
}

/// **Runs still live that match**, which no digest line records yet: matched on their metadata
/// and, for a root, its kept prompt, read through the hardened opener. A child's task is in its
/// contract, which is written when it ends.
fn live_rows(
    project: &Project,
    index: usize,
    live: &[NodeSummary],
    m: &Matcher,
    found: &[Found],
) -> Vec<Row> {
    let scrub = marion_supervisor::export::scrub::Scrubber::from_process([]);
    live.iter()
        .rev()
        .filter(|n| !found.iter().any(|f| f.hit.run.agent_id == n.agent_id))
        .filter_map(|n| {
            let state = live_state(n.state)?;
            let mut run = Run::bare(n.agent_id.clone(), &n.agent_type, n.harness.as_str());
            run.parent_id = n.parent_id.clone();
            run.started = n.started_at;
            run.tokens = n.tokens;
            if n.parent_id.is_none()
                && let Ok(bytes) = read_agent_file(
                    project.dir.path(),
                    &n.agent_id.0,
                    &["prompt.txt"],
                    Kind::Prompt,
                )
            {
                let mut prompt = scrub.clean(&String::from_utf8_lossy(&bytes));
                let mut at = prompt.len().min(PROMPT_CAP);
                while !prompt.is_char_boundary(at) {
                    at -= 1;
                }
                prompt.truncate(at);
                run.prompt = prompt;
            }
            let hit = m.confirm_live(&run, state)?;
            Some(Row {
                found: Found {
                    project: index,
                    hit,
                },
                live: Some(state),
            })
        })
        .collect()
}

fn print_rows(rows: &[Row], projects: &[Project], all: bool, json: bool, now: u64) {
    let names = search::shown_names(projects);
    for row in rows {
        let (project, name) = (&projects[row.found.project], &names[row.found.project]);
        if json {
            out!("{}", json_ascii(&json_row(row, project, name).to_string()));
        } else {
            let column = all.then_some(name.as_str());
            out!("{}", text_row(row, project, column, now));
        }
    }
}

/// `state:`'s word for a row: its live state, else how it ended, `landed` for an `ok` that landed.
fn state_word(row: &Row) -> &'static str {
    let run = &row.found.hit.run;
    match row.live {
        Some(s) => s.word(),
        None if run.landed => State::Landed.word(),
        None => run.status.map_or("unknown", |s| State::of_exit(s).word()),
    }
}

fn glyph(word: &str) -> char {
    match word {
        "ok" | "landed" => '✓',
        "running" | "waiting" => '●',
        "cancelled" => '⊘',
        "unknown" => '·',
        _ => '✗',
    }
}

/// `acme  3c33  ✓ ok        2d  add token-bucket limiter      …the limiter now refuses…`.
/// `name` is the project column, shown only when every project is searched.
fn text_row(row: &Row, project: &Project, name: Option<&str>, now: u64) -> String {
    let hit: &Hit = &row.found.hit;
    let run = &hit.run;
    let word = state_word(row);
    let when = run.ended.or(run.started).unwrap_or(run.at);
    let age = ago(now.saturating_sub(unix_ms(when)) / 1000);
    let mut line = String::new();
    if let Some(name) = name {
        let gone = if project.gone() { " (gone)" } else { "" };
        line.push_str(&format!("{:<14}  ", format!("{name}{gone}")));
    }
    line.push_str(&format!(
        "{:<4}  {} {:<9} {:>4}  {:<30}",
        marion_supervisor::node_label::short_id(&run.agent_id.0),
        glyph(word),
        word,
        age,
        title(&run.prompt),
    ));
    if !hit.snippet.is_empty() {
        line.push_str("  ");
        if !matches!(hit.field, "" | "prompt" | "report") {
            line.push_str(hit.field);
            line.push_str(": ");
        }
        line.push_str(&hit.snippet);
    }
    line.trim_end().to_string()
}

/// marion's own facts at the top level; everything an agent or a person wrote under `untrusted`,
/// the field's name being the warning to whatever reads it.
fn json_row(row: &Row, project: &Project, name: &str) -> serde_json::Value {
    let hit = &row.found.hit;
    let run = &hit.run;
    serde_json::json!({
        "project": name,
        "root": project.root,
        "gone": project.gone(),
        "agent_id": run.agent_id,
        "task_id": run.task_id,
        "parent_id": run.parent_id,
        "agent_type": run.agent_type,
        "harness": run.harness,
        "model": run.model,
        "state": state_word(row),
        "started": run.started,
        "ended": run.ended,
        "tokens": run.tokens,
        "field": hit.field,
        "untrusted": {
            "title": title(&run.prompt),
            "snippet": hit.snippet,
        },
    })
}

/// `json` with every non-ASCII character as its `\uXXXX` escape: still the same JSON, and nothing
/// left in it for a terminal to read as a control, a bidi override or an invisible character.
fn json_ascii(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// The first line of the task, as the row's title, cut to fit its column.
fn title(prompt: &str) -> String {
    const WIDTH: usize = 30;
    let first = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if first.chars().count() <= WIDTH {
        return first.to_string();
    }
    let mut cut: String = first.chars().take(WIDTH - 1).collect();
    cut.push('…');
    cut
}

/// The largest whole unit of `secs`: `45s`, `12m`, `3h`, `2d`, `5w`.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        86_400..1_209_600 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

fn shown_query(words: &[String]) -> String {
    if words.is_empty() {
        "anything".to_string()
    } else {
        format!("\"{}\"", words.join(" "))
    }
}

fn unix_ms(t: marion_core::encoding::SystemTime) -> u64 {
    t.0.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn now_ms() -> u64 {
    unix_ms(marion_core::encoding::SystemTime(
        std::time::SystemTime::now(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::{AgentId, ExitStatus};
    use marion_core::encoding::SystemTime;

    const NOW: u64 = 1_792_108_800_000;

    fn words(w: &[&str]) -> Vec<String> {
        std::iter::once("search")
            .chain(w.iter().copied())
            .map(str::to_string)
            .collect()
    }

    fn row(prompt: &str, field: &'static str, snippet: &str) -> Row {
        let mut run = Run::bare(
            AgentId("019fbf94-3c33-7000-8000-000000000001".into()),
            "codex",
            "codex",
        );
        run.status = Some(ExitStatus::Ok);
        run.prompt = prompt.into();
        run.ended = Some(SystemTime::from_unix_millis(NOW - 2 * 86_400_000));
        Row {
            found: Found {
                project: 0,
                hit: Hit {
                    run,
                    field,
                    snippet: snippet.into(),
                },
            },
            live: None,
        }
    }

    fn project() -> Project {
        Project {
            dir: marion_core::paths::ProjectDir::from_hash(Path::new("/s"), "0123456789ab"),
            root: Some("/nonexistent-search-test/acme/.git".into()),
        }
    }

    #[test]
    fn its_words_parse() {
        let a = parse(&words(&[
            "limiter",
            "--json",
            "--limit",
            "5",
            "--reindex",
            "--all",
        ]))
        .unwrap();
        assert_eq!(a.words, ["limiter"]);
        assert!(a.json && a.reindex && a.all);
        assert_eq!(a.limit, Some(5));
        assert_eq!(parse(&words(&["--", "--json"])).unwrap().words, ["--json"]);
        for bad in [
            &["--limit", "0"][..],
            &["--limit", "x"],
            &["--all-of-it"],
            &["--json=1"],
        ] {
            assert!(matches!(parse(&words(bad)), Err(Exit::Usage(_))), "{bad:?}");
        }
        assert_eq!(parse(&words(&["-h"])), Err(Exit::Help));
    }

    #[test]
    fn a_row_is_one_line_in_its_columns() {
        let line = text_row(
            &row(
                "add a token-bucket limiter\nand tests",
                "report",
                "…refuses bursts",
            ),
            &project(),
            None,
            NOW,
        );
        assert_eq!(
            line,
            "3c33  ✓ ok          2d  add a token-bucket limiter      …refuses bursts"
        );
        let paths = text_row(
            &row("x", "paths", "src/a.rs"),
            &project(),
            Some("acme"),
            NOW,
        );
        assert!(paths.starts_with("acme (gone)     3c33"), "{paths}");
        assert!(paths.ends_with("paths: src/a.rs"), "{paths}");
    }

    /// **A prompt cannot forge a row**: a newline, a CR, a bidi override and a zero-width space are
    /// spelled, not obeyed, once the row passes through `out!`'s line.
    #[test]
    fn a_hostile_prompt_stays_on_one_visible_row() {
        let line = text_row(
            &row(
                "ok\r3c34 ✓ \u{202e}gnp.exe\u{200b}\nsecond",
                "prompt",
                "x\ny",
            ),
            &project(),
            None,
            NOW,
        );
        let shown = marion_supervisor::printable::line(&line);
        assert_eq!(shown.matches('\n').count(), 1, "{shown}");
        assert!(!shown.contains(['\r', '\u{202e}', '\u{200b}']), "{shown}");
        assert!(shown.contains("\\u{202e}"), "{shown}");
    }

    /// Recorded text sits under `untrusted`, and the printed JSON is ASCII and still JSON.
    #[test]
    fn json_keeps_recorded_text_under_untrusted() {
        let r = row("do \u{202e}this\nnow", "report", "said \"hi\"");
        let printed = json_ascii(&json_row(&r, &project(), "acme").to_string());
        assert!(printed.is_ascii(), "{printed}");
        let v: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(v["untrusted"]["title"], "do \u{202e}this");
        assert_eq!(v["untrusted"]["snippet"], "said \"hi\"");
        assert_eq!(v["state"], "ok");
        assert_eq!(v["project"], "acme");
        assert_eq!(v["gone"], true);
        assert!(v.get("prompt").is_none() && v.get("snippet").is_none());
        assert_eq!(json_ascii("\u{1f600}"), "\\ud83d\\ude00");
    }

    #[test]
    fn ages_and_titles_are_short() {
        assert_eq!(ago(59), "59s");
        assert_eq!(ago(3599), "59m");
        assert_eq!(ago(7200), "2h");
        assert_eq!(ago(10 * 86_400), "10d");
        assert_eq!(ago(30 * 86_400), "4w");
        assert_eq!(title("\n  first line  \nsecond"), "first line");
        assert_eq!(title(&"a".repeat(40)).chars().count(), 30);
    }

    #[test]
    fn a_live_state_is_running_or_waiting() {
        assert_eq!(live_state(NodeState::Running), Some(State::Running));
        assert_eq!(live_state(NodeState::Idle), Some(State::Waiting));
        assert_eq!(live_state(NodeState::Exited(ExitStatus::Ok)), None);
    }
}
