//! **A race, end to end: three canned claude seats, one of which does the work.**
//!
//! One run of real binaries against marion's canned provider. A root is started over the socket and
//! a spawn naming it as caller asks for a race of three `claude` seats, each on its own model, under
//! one verification line, `test -f ok`. The canned provider tells the seats apart by the model each
//! one's harness sends: seat 2 writes `ok`, seats 1 and 3 write a different file, and all three
//! report. marion runs `test -f ok` in each seat's worktree and decides.
//!
//! What is asserted: seat 2 wins by verification and the others fail; every seat's work landed on
//! its own branch; the race was decided only after every seat had exited, and its scoreboard file
//! and journal record agree; the tree carries each seat's badge.

use std::process::Command;
use std::time::Duration;

use marion_core::contract::AgentId;
use marion_core::journal::RecordKind;
use marion_core::paths::ProjectDir;
use marion_core::proto::notify::Event as Note;
use marion_core::proto::params::AgentSpawnParams;
use marion_core::proto::{Call, Method, MethodResult, Outcome, SpawnCaller};
use marion_core::race::{DecidedBy, SeatVerdict};
use marion_core::secret::Secret;
use marion_provider::{CannedServer, Config, NodeScript, Script, ScriptedCall};
use marion_supervisor::socket::project_root;
use marion_testsupport::{fixture_repo, scratch};
use serde_json::json;

mod common;
use common::client::{Client, paths_for};
use common::journal::records;

const BOUND: Duration = Duration::from_secs(240);
const ROOT_MARKER: &str = "MARION-RACE-ROOT-TURN-9b2d";
/// Each seat's model, which is also what the canned provider keys its script on: a seat's request
/// carries its own model and no other seat's.
const MODELS: [&str; 3] = ["race-model-one", "race-model-two", "race-model-three"];
const WINNER_FILE: &str = "ok";

fn script() -> Script {
    let claude = marion_harness::adapter_for(marion_core::harness::Harness::ClaudeCode)
        .expect("claude has an adapter");
    let seat = |i: usize| {
        let file = if i == 1 {
            WINNER_FILE.to_string()
        } else {
            format!("wrong-{i}.txt")
        };
        NodeScript {
            marker: MODELS[i].into(),
            call_prefix: format!("seat{i}"),
            turns: vec![
                ScriptedCall::new(
                    "Write",
                    json!({"file_path": file, "content": "seat work\n"}),
                ),
                ScriptedCall::new(
                    claude.marion_tool_name("report"),
                    json!({"narrative": format!("wrote {file}")}),
                ),
            ],
            final_text: "Done.".into(),
        }
    };
    Script {
        nodes: vec![
            NodeScript {
                marker: ROOT_MARKER.into(),
                call_prefix: "root".into(),
                turns: vec![],
                final_text: "Waiting on the race.".into(),
            },
            seat(0),
            seat(1),
            seat(2),
        ],
        ..Script::default()
    }
}

fn race_params(caller: SpawnCaller) -> AgentSpawnParams {
    AgentSpawnParams {
        wider_children: None,
        review_of: None,
        notify_parent: false,
        agent_type: String::new(),
        prompt: "Create the file the task needs at the repository root, then report.".into(),
        native_launch: None,
        caller: Some(caller),
        repo: None,
        acceptance_criteria: vec![],
        verification: vec![format!("test -f {WINNER_FILE}")],
        writable_scope: vec![],
        timeout_secs: Some(120),
        model: None,
        no_change_record: None,
        pane: None,
        isolation: None,
        allow_concurrent_writes: None,
        profile: None,
        candidates: MODELS.iter().map(|m| format!("claude:{m}")).collect(),
        race: None,
        budget_tokens: None,
    }
}

/// Parks every request whose body names one of `markers` until released: the root, so it is still
/// running (and so still wants its race) until the race is decided, and in the `first` test a seat
/// that never finishes on its own.
#[derive(Debug)]
struct Parked {
    markers: Vec<String>,
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
}

impl marion_provider::Hold for Parked {
    fn wait_for(&self, _wire: Option<&str>, body: &serde_json::Value) {
        let body = body.to_string();
        if !self.markers.iter().any(|m| body.contains(m.as_str())) {
            return;
        }
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }
}

impl Parked {
    fn new(markers: &[&str]) -> std::sync::Arc<Parked> {
        std::sync::Arc::new(Parked {
            markers: markers.iter().map(|m| m.to_string()).collect(),
            released: std::sync::Mutex::new(false),
            wake: std::sync::Condvar::new(),
        })
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

/// Releases the parked requests however the bed ends, so no provider thread is left waiting.
struct Release(std::sync::Arc<Parked>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Everything one race run leaves for its assertions: the bed stays up until this is dropped.
struct Ran {
    repo: std::path::PathBuf,
    state: std::path::PathBuf,
    project: ProjectDir,
    root: AgentId,
    c: Client,
    started: marion_core::proto::result::RaceStarted,
    result: marion_core::race::RaceResult,
    // Dropped last: the supervisor stops before the provider goes.
    _sup: Stopped,
    _server: CannedServer,
    /// Last of all: the directory every other field lives in.
    _dir: marion_testsupport::Scratch,
}

struct Stopped(common::Supervisor);
impl Drop for Stopped {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Start the canned provider and a supervisor, a root, and the race `params` asks for on the
/// root's behalf; return once the race is decided. The root's turn is parked until then, so its
/// race is never abandoned; so is every seat whose model is in `park`. `None` where claude is not
/// installed.
fn run_race(
    tag: &str,
    script: Script,
    park: &[&str],
    params: impl FnOnce(SpawnCaller) -> AgentSpawnParams,
) -> Option<Ran> {
    if !marion_testsupport::harness_available("claude") {
        return None;
    }
    let dir = scratch(tag);
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let key = project_root(&repo);
    let project = ProjectDir::new(&state, &key);
    let parked = Parked::new(&[&[ROOT_MARKER], park].concat());
    let release = Release(parked.clone());
    let server = CannedServer::start_held(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script,
        },
        Some(parked as std::sync::Arc<dyn marion_provider::Hold>),
    )
    .expect("the canned provider binds");
    let sup = Stopped(common::Supervisor::start(
        &state,
        &key,
        &std::env::var("PATH").unwrap_or_default(),
        &server.base_url(),
        BOUND,
    ));
    let paths = paths_for(&state, &repo);
    let mut c = Client::dial(&paths);
    let root = c.spawn_root(&repo, &format!("{ROOT_MARKER}: wait for the race."), 120);
    let token = common::declaration_of(&state, &root)["MARION_NODE_TOKEN"].clone();

    // A second connection follows the tree, so the decision is observed as it is announced.
    let mut watcher = Client::dial(&paths);
    watcher.read_bound(BOUND);
    watcher.tree();

    let id = c.send(Call::AgentSpawn(params(SpawnCaller {
        agent_id: root.clone(),
        node_token: Secret::new(token),
    })));
    let (_, outcome) = c.read_to_response(id);
    let Outcome::Result(body) = outcome else {
        panic!("the race was refused: {outcome:?}")
    };
    let MethodResult::AgentSpawn(spawned) = Method::AgentSpawn.decode_result(&body).unwrap() else {
        panic!("wrong result")
    };
    let started = spawned.race.expect("a race answers with its seats");
    // Any seat's verdict, not only a winner's: a race nobody won must fail below, naming its
    // scoreboard, rather than here as a timeout.
    let decided = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        watcher.wait_for_event(|n| {
            matches!(n, Note::NodeAdded { node, .. }
                if node.race.as_ref().is_some_and(|b| b.verdict.is_some()))
        })
    }));
    if decided.is_err() {
        let kinds: Vec<String> = records(&project.journal())
            .iter()
            .map(|k| format!("{k:?}").chars().take(160).collect())
            .collect();
        panic!("the race was never decided; journal:\n{}", kinds.join("\n"));
    }

    let result = marion_supervisor::race::read_result(&project, &started.race_id)
        .expect("the scoreboard is on disk once the decision is announced");
    drop(release);
    Some(Ran {
        repo,
        state,
        project,
        root,
        c,
        started,
        result,
        _sup: sup,
        _server: server,
        _dir: dir,
    })
}

#[test]
fn three_canned_seats_race_and_the_one_that_passes_verification_wins() {
    let Some(Ran {
        repo,
        state,
        project,
        root,
        mut c,
        started,
        result,
        // Named, never `..`: a field a pattern skips is dropped at once, and these hold the bed
        // up. Bindings drop in reverse, so the supervisor stops before its directory goes.
        _dir,
        _server,
        _sup,
    }) = run_race("race-e2e", script(), &[], race_params)
    else {
        return;
    };
    assert_eq!(started.seats.len(), 3);
    assert!(
        started
            .seats
            .iter()
            .all(|s| s.agent_id.is_some() && s.refused.is_none()),
        "every seat started: {:?}",
        started.seats
    );
    assert_eq!(started.seats[1].candidate, "claude:race-model-two");

    assert_eq!(result.winner, Some(2), "{}", result.scoreboard());
    assert_eq!(result.decided_by, DecidedBy::Verification);
    assert_eq!(
        result.seats.iter().map(|r| r.verdict).collect::<Vec<_>>(),
        vec![SeatVerdict::Failed, SeatVerdict::Won, SeatVerdict::Failed],
        "{}",
        result.scoreboard()
    );
    assert_eq!(result.seats[1].verified, (1, 1));
    assert_eq!(result.seats[0].verified, (0, 1));
    assert_eq!(result.requester, root);
    let models: Vec<_> = result.seats.iter().map(|r| r.model.as_deref()).collect();
    assert_eq!(models, MODELS.map(Some).to_vec());

    // Every seat's work is on its own branch, and each branch is the one its row names.
    let branches = String::from_utf8(
        Command::new("git")
            .current_dir(&repo)
            .args(["branch", "--list", "marion/*", "--format=%(refname:short)"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    for row in &result.seats {
        let b = row.branch.as_deref().expect("every seat's work landed");
        assert!(branches.lines().any(|l| l == b), "{b} in {branches}");
    }

    // Decided after every seat exited, and the journal's record agrees with the file.
    let journal = records(&project.journal());
    let seats: Vec<AgentId> = started
        .seats
        .iter()
        .filter_map(|s| s.agent_id.clone())
        .collect();
    let decided_at = journal
        .iter()
        .position(|k| matches!(k, RecordKind::RaceDecided(_)))
        .expect("the decision is journaled");
    for seat in &seats {
        let exited = journal
            .iter()
            .position(|k| matches!(k, RecordKind::Exited(e) if &e.agent_id == seat))
            .expect("every seat exited");
        assert!(
            exited < decided_at,
            "a seat exited after the race was decided"
        );
    }
    let RecordKind::RaceDecided(d) = &journal[decided_at] else {
        unreachable!()
    };
    assert_eq!(d.winner, seats.get(1).cloned());
    let opened = journal
        .iter()
        .position(|k| matches!(k, RecordKind::RaceOpened(_)))
        .expect("the race was opened");
    let first_seat = journal
        .iter()
        .position(|k| matches!(k, RecordKind::SpawnIntent(i) if i.race.is_some()))
        .unwrap();
    assert!(
        opened < first_seat,
        "the race is opened before its first seat"
    );

    // Refused before anything is written: each of these leaves the journal with the one race.
    type Edit = Box<dyn Fn(&mut AgentSpawnParams)>;
    let refusals: Vec<(&str, Edit)> = vec![
        ("not both", Box::new(|p| p.agent_type = "claude".into())),
        ("its own model", Box::new(|p| p.model = Some("m".into()))),
        ("needs verification", Box::new(|p| p.verification.clear())),
        ("at least 2", Box::new(|p| p.candidates.truncate(1))),
        (
            "no agent type is named",
            Box::new(|p| p.candidates[0] = "nosuch".into()),
        ),
        (
            "own worktree",
            Box::new(|p| p.isolation = Some(marion_core::contract::Isolation::SharedCwd)),
        ),
        (
            "max_concurrent_children",
            Box::new(|p| {
                p.candidates = vec!["claude".into(); 5];
            }),
        ),
    ];
    for (why, edit) in refusals {
        let mut p = race_params(SpawnCaller {
            agent_id: root.clone(),
            node_token: Secret::new(
                common::declaration_of(&state, &root)["MARION_NODE_TOKEN"].clone(),
            ),
        });
        edit(&mut p);
        let id = c.send(Call::AgentSpawn(p));
        let (_, outcome) = c.read_to_response(id);
        let Outcome::Error(e) = outcome else {
            panic!("a race that should be refused ({why}) was served: {outcome:?}")
        };
        assert!(e.message.contains(why), "{why}: {}", e.message);
    }
    let opened = records(&project.journal())
        .iter()
        .filter(|k| matches!(k, RecordKind::RaceOpened(_)))
        .count();
    assert_eq!(opened, 1, "a refused race opened nothing");

    // The tree names every seat's race and verdict.
    let tree = c.tree();
    for (i, seat) in seats.iter().enumerate() {
        let badge = tree
            .iter()
            .find(|n| &n.agent_id == seat)
            .and_then(|n| n.race.clone())
            .expect("a seat's summary carries its badge");
        assert_eq!(badge.seat as usize, i + 1);
        assert_eq!(badge.race_id, started.race_id);
    }
}

/// The refs under `marion/` in `repo`.
fn task_branches(repo: &std::path::Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["branch", "--list", "marion/*", "--format=%(refname:short)"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// **`losers: prune` deletes the losers' branches and only theirs**, once a winner is decided:
/// the scoreboard marks what is actually gone, and the winner's branch stays to be merged.
#[test]
fn a_pruned_race_deletes_only_the_losers_branches() {
    let Some(ran) = run_race("race-prune", script(), &[], |caller| AgentSpawnParams {
        race: Some(marion_core::race::RawRacePolicy {
            losers: Some(marion_core::race::Losers::Prune),
            ..Default::default()
        }),
        ..race_params(caller)
    }) else {
        return;
    };
    let r = &ran.result;
    assert_eq!(r.winner, Some(2), "{}", r.scoreboard());
    let left = task_branches(&ran.repo);
    for row in &r.seats {
        let branch = row.branch.clone().expect("every seat's work landed");
        if row.seat == 2 {
            assert!(!row.pruned);
            assert!(
                left.contains(&branch),
                "the winner's {branch} stays: {left:?}"
            );
        } else {
            assert!(row.pruned, "{}", r.scoreboard());
            assert!(
                !left.contains(&branch),
                "a loser's {branch} is gone: {left:?}"
            );
        }
    }
}

/// **`first` stops the seats still running once one passes**, through marion's own stop: the held
/// seat never answers, so it can end before its 120 s timeout only by being stopped, and a stop
/// records it cancelled, never failed or timed out.
#[test]
fn a_first_race_stops_the_seats_still_running_once_one_passes() {
    let ran = run_race("race-first", script(), &[MODELS[0]], |caller| {
        AgentSpawnParams {
            candidates: MODELS[..2].iter().map(|m| format!("claude:{m}")).collect(),
            race: Some(marion_core::race::RawRacePolicy {
                first: Some(true),
                ..Default::default()
            }),
            ..race_params(caller)
        }
    });
    let Some(ran) = ran else {
        return;
    };
    let r = &ran.result;
    assert_eq!(r.winner, Some(2), "{}", r.scoreboard());
    assert_eq!(r.decided_by, DecidedBy::FirstPass, "{}", r.scoreboard());
    assert_eq!(
        r.seats[0].verdict,
        SeatVerdict::Cancelled,
        "the held seat was stopped: {}",
        r.scoreboard()
    );
    let held = ran.started.seats[0]
        .agent_id
        .clone()
        .expect("seat 1 started");
    let stopped = records(&ran.project.journal())
        .into_iter()
        .any(|k| matches!(k, RecordKind::KillConfirmed(k) if k.agent_id == held));
    assert!(stopped, "marion stopped the held seat itself");
}
