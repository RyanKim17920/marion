//! **The home screen, end to end**: bare `marion` on a pty this test owns, read back through a
//! terminal emulator the way an operator's eye reads it.
//!
//! The bed is a `codex` shim on a stated `PATH`, as `node_kill.rs` builds one, so it needs no
//! harness, no network and no credential. Run headless it is the app-server shim
//! (`common::app_server`), whose turn prints a `report` call's app-server items a line at a time —
//! so the preview's output has frames to show, arriving while the node runs — and, run in a pane,
//! it prints a banner and echoes what is typed into it. Both hold until the test opens a gate file.
//!
//! What it proves, in the order an operator meets it: the agents page opens empty; `n`, `tab` to
//! the codex type, a typed task and Enter run `marion run … --detach`, and the node appears in the
//! list; its calls appear in the preview as they happen; `s` sends a steer and the supervisor's
//! answer lands on the footer; `x` asks before it cancels; `,` is Setup and `?` the keys; and Enter
//! on a pane node attaches, `^] d` comes back.
//!
//! ```sh
//! cargo test -p marion-supervisor --test it_canned home_e2e::
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_supervisor::pty::{PtyHost, PtyMaster, StdinPlan, WinSize, spawn_pty};
use marion_testsupport::{Scratch, fixture_repo, scratch, sweep};

use crate::common;
use common::cast::cast_records;

const BOUND: Duration = Duration::from_secs(90);
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
const SIZE: WinSize = WinSize {
    cols: 120,
    rows: 40,
};
const TASK: &str = "HOMEE2E-TASK list the limiter files";

/// A `codex` that streams a `report` call's app-server items when headless, is a tiny echoing TUI
/// when run in a pane, records how it was launched either way, and holds until `gate` exists.
fn shim(bin: &Path, gate: &Path) {
    let started = r#"{"method":"item/started","params":{"item":{"type":"mcpToolCall","id":"call_home","server":"marion","tool":"report","status":"inProgress","arguments":{"narrative":"s6 probe: reporting via MCP"}}}}"#;
    let completed = r#"{"method":"item/completed","params":{"item":{"type":"mcpToolCall","id":"call_home","server":"marion","tool":"report","status":"completed","arguments":{"narrative":"s6 probe: reporting via MCP"}}}}"#;
    let said = r#"{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"msg_home","text":"done"}}}"#;
    let script = format!(
        r#"case "$1" in
  --version) echo "codex-cli 0.146.0"; exit 0 ;;
esac
mkdir -p {argv}
if [ -n "$MARION_SHIM_TURN" ]; then
  # What marion launched the server with, and the node token from its environment — a live codex
  # node's token rides the environment, never argv — under the server's pid.
  printf '%s TOKEN_FROM_ENV="%s"\n' "$MARION_SHIM_ARGV" "$(cat "${{MARION_NODE_TOKEN_FILE:-/dev/null}}")" > {argv}/$PPID
  # The turn's first call, then the rest only once the gate opens, so the node is still running
  # while the test steers and cancels it.
  sleep 0.3; echo '{started}'
  waited=0
  while [ ! -e {gate} ] && [ "$waited" -le 6000 ]; do sleep 0.05; waited=$((waited + 1)); done
  echo '{completed}'; echo '{said}'
  exit 0
fi
# A pane node's launch and token, as the headless branch records them: `n` runs a codex task in a
# pane, since doctor measures one for it, and the test spawns a child under it.
printf '%s TOKEN_FROM_ENV="%s"\n' "$*" "$(cat "${{MARION_NODE_TOKEN_FILE:-/dev/null}}")" > {argv}/$$
echo "HOMEE2E-PANE-READY"
( while [ ! -e {gate} ]; do sleep 0.1; done; kill $$ ) &
while IFS= read -r typed; do echo "typed: $typed"; done
waited=0
while [ ! -e {gate} ] && [ "$waited" -le 1200 ]; do sleep 0.05; waited=$((waited + 1)); done
exit 0
"#,
        gate = common::shell_quote(gate),
        argv = common::shell_quote(&bin.join("../argv")),
    );
    common::app_server::fake_codex(bin, &script);
}

/// This test's own credential store: a file under its scratch directory, never the operator's
/// Keychain, so Setup's LOGINS section lists what the test stored and nothing of the machine's.
fn isolated_keys(cmd: &mut Command, dir: &Path) {
    cmd.env("MARION_CREDENTIAL_STORE", "file")
        .env("XDG_CONFIG_HOME", dir.join("config"));
}

/// A key that is not a key, stored the way an operator stores one, for Setup to list by id.
const FAKE_KEY: &str = "sk-HOMEE2E-never-shown";

fn store_fake_key(dir: &Path, id: &str) {
    use std::io::Write;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    cmd.args(["login", id, "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null());
    isolated_keys(&mut cmd, dir);
    let mut child = cmd.spawn().expect("marion login starts");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(FAKE_KEY.as_bytes())
        .unwrap();
    assert!(
        child.wait().unwrap().success(),
        "marion login --stdin stores it"
    );
}

struct Operator {
    host: PtyHost,
    cast: PathBuf,
}

impl Operator {
    fn start(dir: &Path, repo: &Path, state: &Path, path: &str) -> Operator {
        let cast = dir.join("operator.cast");
        let master = PtyMaster::open(SIZE).expect("the operator's pty");
        let host = PtyHost::start(
            AgentId("operator".into()),
            master,
            &cast,
            SIZE,
            "xterm-256color",
            Instant::now(),
        )
        .expect("recording the operator's screen");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
        cmd.current_dir(repo)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("PATH", path)
            .env("MARION_STATE_DIR", state)
            .env("COPILOT_AUTO_UPDATE", "false");
        isolated_keys(&mut cmd, dir);
        let witness = marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("a display plane");
        let child = spawn_pty(
            witness,
            &mut cmd,
            host.master(),
            StdinPlan::TerminalSlave,
            None,
        )
        .expect("marion starts on the operator's pty");
        host.adopt(child);
        Operator { host, cast }
    }

    /// The grid as the operator reads it.
    fn screen(&self) -> String {
        let mut term = marion_term::Term::with_options(
            marion_term::Size::new(SIZE.cols as usize, SIZE.rows as usize),
            marion_tui::grid_options(),
        );
        for (code, data) in cast_records(&self.cast) {
            if code == "o" {
                term.advance(data.as_bytes());
            }
        }
        term.viewport_lines().join("\n")
    }

    /// With `MARION_HOME_DUMP=<dir>` set, the screen's cells — colour and modifiers included, as
    /// the emulator holds them — as `<dir>/<name>.json`, for a rasteriser to draw.
    fn dump(&self, name: &str) {
        let Some(dir) = std::env::var_os("MARION_HOME_DUMP") else {
            return;
        };
        use ratatui::style::{Color, Modifier};
        let mut term = marion_term::Term::with_options(
            marion_term::Size::new(SIZE.cols as usize, SIZE.rows as usize),
            marion_tui::grid_options(),
        );
        for (code, data) in cast_records(&self.cast) {
            if code == "o" {
                term.advance(data.as_bytes());
            }
        }
        let area = ratatui::layout::Rect::new(0, 0, SIZE.cols, SIZE.rows);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&term, area, &mut buf);
        let tag = |c: Color| match c {
            Color::Reset => "d".to_string(),
            Color::Indexed(n) => format!("i{n}"),
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            Color::Black => "a0".into(),
            Color::Red => "a1".into(),
            Color::Green => "a2".into(),
            Color::Yellow => "a3".into(),
            Color::Blue => "a4".into(),
            Color::Magenta => "a5".into(),
            Color::Cyan => "a6".into(),
            Color::Gray => "a7".into(),
            Color::DarkGray => "a8".into(),
            Color::LightRed => "a9".into(),
            Color::LightGreen => "a10".into(),
            Color::LightYellow => "a11".into(),
            Color::LightBlue => "a12".into(),
            Color::LightMagenta => "a13".into(),
            Color::LightCyan => "a14".into(),
            Color::White => "a15".into(),
        };
        let rows: Vec<serde_json::Value> = (0..SIZE.rows)
            .map(|y| {
                serde_json::Value::Array(
                    (0..SIZE.cols)
                        .map(|x| {
                            let c = &buf[(x, y)];
                            let mut f = String::new();
                            for (m, ch) in [
                                (Modifier::BOLD, 'B'),
                                (Modifier::DIM, 'D'),
                                (Modifier::REVERSED, 'R'),
                            ] {
                                if c.modifier.contains(m) {
                                    f.push(ch);
                                }
                            }
                            serde_json::json!([c.symbol(), tag(c.fg), tag(c.bg), f])
                        })
                        .collect(),
                )
            })
            .collect();
        let doc = serde_json::json!({"w": SIZE.cols, "h": SIZE.rows, "rows": rows});
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            Path::new(&dir).join(format!("{name}.json")),
            doc.to_string(),
        )
        .unwrap();
    }

    fn type_in(&self, bytes: &[u8]) {
        self.host
            .master()
            .write_all(bytes)
            .unwrap_or_else(|e| panic!("typing {bytes:?}: {e}\n{}", self.screen()));
    }

    /// Wait until `seen` holds of the screen.
    fn wait_for(&self, what: &str, seen: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + BOUND;
        loop {
            let s = self.screen();
            if seen(&s) {
                eprintln!(
                    "saw {what} after {:?}",
                    START.get_or_init(Instant::now).elapsed()
                );
                return s;
            }
            assert!(
                Instant::now() < deadline,
                "never saw {what}. The screen:\n{s}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct Bed {
    _scratch: Scratch,
    dir: PathBuf,
    repo: PathBuf,
    state: PathBuf,
    gate: PathBuf,
    path: String,
}

impl Bed {
    fn new(tag: &str) -> Bed {
        let s = scratch(tag);
        let dir = s.to_path_buf();
        let repo = fixture_repo(&dir);
        let state = dir.join("state");
        let bin = dir.join("bin");
        let gate = dir.join("gate");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        shim(&bin, &gate);
        for sys in ["/usr/bin", "/bin"] {
            assert!(
                !Path::new(sys).join("codex").exists(),
                "{sys} holds a `codex`, so this bed could launch a real harness"
            );
        }
        let path = format!("{}:/usr/bin:/bin", bin.to_string_lossy());
        Bed {
            _scratch: s,
            dir,
            repo,
            state,
            gate,
            path,
        }
    }

    fn marion(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", &self.path)
            .env("MARION_STATE_DIR", &self.state)
            .output()
            .expect("marion runs")
    }
}

impl Bed {
    /// The one root `marion ls` lists (without a terminal it prints `marion list`'s lines).
    fn root_id(&self) -> AgentId {
        let out = self.marion(&["ls"]);
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        text.lines()
            .filter(|l| !l.contains(" parent "))
            .flat_map(str::split_whitespace)
            .find(|w| w.len() == 36 && w.matches('-').count() == 4)
            .map(|w| AgentId(w.to_string()))
            .unwrap_or_else(|| panic!("no root listed: {text}"))
    }

    fn spawn_child(&self, parent: &AgentId, prompt: &str) -> AgentId {
        // The root's identity rides its argv and its token its environment; the shim wrote both
        // down as it started.
        let want = format!("MARION_AGENT_ID=\"{}\"", parent.0);
        let key = "TOKEN_FROM_ENV=\"";
        let deadline = Instant::now() + BOUND;
        let token = loop {
            let found = std::fs::read_dir(self.dir.join("argv"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .find(|a| a.contains(&want))
                .and_then(|a| {
                    let at = a.find(key)? + key.len();
                    a[at..].split('"').next().map(str::to_string)
                });
            if let Some(t) = found {
                break t;
            }
            assert!(
                Instant::now() < deadline,
                "the root never started with its token"
            );
            std::thread::sleep(Duration::from_millis(200));
        };
        let mut c =
            common::client::Client::dial(&common::client::paths_for(&self.state, &self.repo));
        let id = c.send(marion_core::proto::Call::AgentSpawn(
            marion_core::proto::params::AgentSpawnParams {
                wider_children: None,
                budget_tokens: None,
                review_of: None,
                notify_parent: false,
                agent_type: "codex".into(),
                prompt: prompt.into(),
                native_launch: None,
                caller: Some(marion_core::proto::SpawnCaller {
                    agent_id: parent.clone(),
                    node_token: token.into(),
                }),
                repo: None,
                acceptance_criteria: vec!["the limiter files are listed".into()],
                verification: vec![],
                writable_scope: vec!["src/**".into()],
                timeout_secs: Some(300),
                model: None,
                no_change_record: None,
                pane: None,
                isolation: None,
                allow_concurrent_writes: None,
                profile: None,
                candidates: vec![],
                race: None,
            },
        ));
        let (_, outcome) = c.read_to_response(id);
        let marion_core::proto::Outcome::Result(body) = outcome else {
            panic!("agent/spawn was refused: {outcome:?}")
        };
        let marion_core::proto::MethodResult::AgentSpawn(r) =
            marion_core::proto::Method::AgentSpawn
                .decode_result(&body)
                .unwrap()
        else {
            panic!("agent/spawn answers with an agent/spawn result")
        };
        r.agent_id
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gate, b"");
        // Every process this bed started carries its scratch path in its argv or cwd.
        std::thread::sleep(Duration::from_millis(300));
        sweep(&self.dir.to_string_lossy());
    }
}

/// The footer's input line for a new codex task, with nothing in the way: `new codex ›`, which a
/// `codex-orchestrator` line or one still checking the harness does not read as.
fn on_codex(s: &str) -> bool {
    s.lines().any(|l| l.trim_start().starts_with("new codex ›"))
}

/// The first line: `marion  <project>`, then the counts.
fn status_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// n → run → list → preview output → steer → confirm-then-cancel → Setup → keys, on one screen.
#[test]
fn n_runs_a_task_and_the_list_shows_it_steers_it_and_cancels_it() {
    let bed = Bed::new("home-e2e-run");
    store_fake_key(&bed.dir, "openrouter:work");
    let op = Operator::start(&bed.dir, &bed.repo, &bed.state, &bed.path);

    // The agents page, empty, offering a new task.
    op.wait_for("the empty agents page", |s| {
        s.contains("no agents") && s.contains("n new")
    });
    // `n`, then `tab` to the codex type, once doctor has found the shim ready.
    op.type_in(b"n");
    op.wait_for("the new-task line", |s| s.contains("enter run"));
    let deadline = Instant::now() + BOUND;
    while !on_codex(&op.screen()) {
        assert!(
            Instant::now() < deadline,
            "never reached a ready codex type:\n{}",
            op.screen()
        );
        op.type_in(b"\t");
        std::thread::sleep(Duration::from_millis(300));
    }
    op.type_in(TASK.as_bytes());
    op.wait_for("the task typed", |s| {
        on_codex(s) && s.contains("HOMEE2E-TASK")
    });
    op.dump("real-new");
    op.type_in(b"\r");

    // The node appears in the list, running.
    op.wait_for("the node in the list", |s| {
        status_line(s).contains("1 running")
    });

    // A child under it, spawned the way the root's own `spawn` would be (its node token): its
    // app-server stream is recorded live, so its call appears while it runs.
    let root = bed.root_id();
    let child = bed.spawn_child(&root, "HOMEE2E-CHILD read the limiter");
    op.wait_for("the child in the list, by its task", |s| {
        s.contains("HOMEE2E-CHILD") && status_line(s).contains("2 running")
    });
    op.type_in(b"j");
    let child_short = marion_supervisor::tree::short_id(&child.0);
    op.wait_for(
        "the child's call in its preview, live, while it runs",
        |s| s.contains(&format!("codex {child_short}")) && s.contains("report \"s6 probe"),
    );
    op.dump("real-agents-child");
    op.type_in(b"k");
    let root_short = marion_supervisor::tree::short_id(&root.0);
    op.wait_for("the root previewed again", |s| {
        s.contains(&format!("codex {root_short}"))
    });

    // Steer: the footer becomes the line, Enter sends, and the supervisor's answer takes it.
    op.type_in(b"s");
    op.wait_for("the steer line", |s| {
        s.contains(&format!("steer codex {root_short}")) && s.contains("enter send")
    });
    op.type_in(b"use a deque");
    op.wait_for("the steer typed", |s| s.contains("use a deque"));
    op.dump("real-agents-steer");
    op.type_in(b"\r");
    op.wait_for("the supervisor's answer to the steer", |s| {
        s.contains("queued") || s.contains("delivered") || s.contains("steer refused")
    });

    // Cancel asks first, naming what it ends below; `n` keeps it, `y` ends it.
    op.type_in(b"x");
    op.wait_for("the confirm", |s| {
        s.contains(&format!("cancel codex {root_short} and the 1 below it?"))
            && s.contains("y / any key")
    });
    op.dump("real-agents-confirm");
    op.type_in(b"n");
    op.wait_for("the refusal to cancel", |s| s.contains("not cancelled"));
    op.type_in(b"x");
    op.wait_for("the confirm again", |s| s.contains("y / any key"));
    op.type_in(b"y");
    op.wait_for("nothing running", |s| {
        !status_line(s).contains("running") && !s.contains("cancelling")
    });

    // Setup on `,`: the real doctor rows and the stored key.
    op.type_in(b",");
    let setup = op.wait_for("Setup with the doctor's answer and the stored key", |s| {
        status_line(s).contains("setup")
            && s.contains("harnesses")
            && s.contains("api keys")
            && !s.contains("checking…")
            && s.contains("openrouter:work")
    });
    assert!(
        !setup.contains("HOMEE2E-never"),
        "a stored key is listed by its id, never shown: {setup}"
    );
    op.dump("real-setup");
    op.type_in(b"?");
    op.wait_for("the key page", |s| {
        status_line(s).contains("keys") && s.contains("input line") && s.contains("cancel, asks")
    });
    op.dump("real-help");

    op.type_in(b"\x03");
}

/// A pane node: Enter attaches to it, `^] d` comes back to the list.
#[test]
fn enter_attaches_to_a_pane_node_and_detaching_returns_home() {
    let bed = Bed::new("home-e2e-pane");
    let out = bed.marion(&["run", "codex", "--prompt", "HOMEE2E-PANE hold", "--pane"]);
    assert!(
        out.status.success(),
        "the pane run did not start: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let op = Operator::start(&bed.dir, &bed.repo, &bed.state, &bed.path);
    op.wait_for("the pane node selected", |s| s.contains("enter attach"));
    op.type_in(b"\r");
    op.wait_for("the pane's own screen", |s| {
        s.contains("HOMEE2E-PANE-READY")
    });
    op.type_in(b"\x1dd");
    op.wait_for("the list again after detaching, repainted whole", |s| {
        s.contains("enter attach") && !s.contains("HOMEE2E-PANE-READY")
    });
    op.type_in(b"\x03");
}

/// `marion run --detach`, `marion ls <id>` and `marion cancel <short>` as plain commands.
#[test]
fn run_detach_ls_and_cancel_are_commands_too() {
    let bed = Bed::new("home-e2e-cli");
    let out = bed.marion(&["run", "codex", "--prompt", "HOMEE2E-CLI hold", "--detach"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("marion: root "))
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or_else(|| panic!("--detach names the root it started: {stdout}"))
        .to_string();
    assert!(
        stdout.contains("detached") && stdout.contains("marion cancel"),
        "{stdout}"
    );

    let ls = bed.marion(&["ls", &id]);
    let text = String::from_utf8_lossy(&ls.stdout);
    assert!(
        ls.status.success(),
        "{}",
        String::from_utf8_lossy(&ls.stderr)
    );
    assert!(
        text.lines().next().is_some_and(|l| l.contains(&id)),
        "{text}"
    );
    let listed = bed.marion(&["ls"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(&id),
        "`marion ls` without a terminal prints the list"
    );

    let short = marion_supervisor::tree::short_id(&id).to_string();
    let cancel = bed.marion(&["cancel", &short]);
    let said = String::from_utf8_lossy(&cancel.stdout);
    assert!(
        cancel.status.success(),
        "{}",
        String::from_utf8_lossy(&cancel.stderr)
    );
    assert!(said.contains("cancelled"), "{said}");
    let again = bed.marion(&["cancel", &short]);
    assert!(
        !again.status.success(),
        "a second cancel is refused: it has already ended"
    );
}
