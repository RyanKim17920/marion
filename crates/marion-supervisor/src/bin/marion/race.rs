//! **`marion race`**: one task, run once per candidate, each seat a contracted node of the
//! operator's in its own worktree under the same verification; the supervisor decides the winner
//! from the seats' own ends. The spawn and the wait are the ones a node's `spawn` tool uses —
//! `courier::spawn` with `candidates`, then `courier::await_race` — with no caller, so the seats sit
//! at the top of the tree.

use std::process::ExitCode;

use marion_core::race::{Losers, RawRacePolicy};
use marion_supervisor::{courier, detach, socket};

use super::cli::{self, Backend, Exit, Place, Word, Words};

/// What `marion race` was asked.
#[derive(Debug, Default)]
pub struct RaceArgs {
    pub prompt: String,
    /// `agent_type[:model]`, in seat order.
    pub on: Vec<String>,
    pub verify: Vec<String>,
    pub first: bool,
    pub prune: bool,
    pub detach: bool,
    pub timeout_secs: Option<u64>,
    pub place: Place,
    pub backend: Backend,
}

pub fn parse(argv: &[String]) -> Result<RaceArgs, Exit> {
    let mut words = Words::after_verb(argv);
    let mut args = RaceArgs::default();
    while let Some(word) = words.next()? {
        match word {
            Word::Flag(f, v) if take(&mut args, f, v, &mut words)? => {}
            Word::Flag(f, v) if args.place.take(f, v, &mut words)? => {}
            Word::Flag(f, v) if args.backend.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(extra) => {
                return Err(Exit::Usage(format!(
                    "unexpected `{extra}`; the task goes in --prompt and the seats in --on"
                )));
            }
        }
    }
    if args.prompt.is_empty() {
        return Err(Exit::Usage("needs --prompt <text>, the task".into()));
    }
    if args.on.len() < 2 {
        return Err(Exit::Usage(
            "needs --on with at least two seats, e.g. `--on claude,codex:gpt-5`".into(),
        ));
    }
    if args.verify.is_empty() {
        return Err(Exit::Usage(
            "needs --verify <command>: a race is decided by the checks each seat's work passes"
                .into(),
        ));
    }
    Ok(args)
}

/// One of `race`'s own flags. `false` when `flag` is not one of them.
fn take(
    args: &mut RaceArgs,
    flag: &str,
    inline: Option<&str>,
    words: &mut Words,
) -> Result<bool, Exit> {
    match flag {
        "--prompt" => args.prompt = words.value(flag, inline)?,
        // Comma-separated, and repeatable: `--on a,b --on c` is three seats.
        "--on" => args.on.extend(
            words
                .value(flag, inline)?
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        ),
        "--verify" => args.verify.push(words.value(flag, inline)?),
        "--first" => args.first = cli::switch(flag, inline)?,
        "--prune" => args.prune = cli::switch(flag, inline)?,
        "--detach" => args.detach = cli::switch(flag, inline)?,
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

/// The race's policy as the flags state it; everything unstated is the repository's `[race]`
/// table's, then marion's default.
fn policy(args: &RaceArgs) -> Option<RawRacePolicy> {
    let policy = RawRacePolicy {
        first: args.first.then_some(true),
        losers: args.prune.then_some(Losers::Prune),
        ..RawRacePolicy::default()
    };
    (policy != RawRacePolicy::default()).then_some(policy)
}

/// The `agent/spawn` a race sends: no caller, the repository named, one seat per candidate.
pub fn params(
    args: &RaceArgs,
    repo: &std::path::Path,
) -> marion_core::proto::params::AgentSpawnParams {
    marion_core::proto::params::AgentSpawnParams {
        wider_children: None,
        budget_tokens: None,
        review_of: None,
        notify_parent: false,
        agent_type: String::new(),
        prompt: args.prompt.clone(),
        native_launch: None,
        caller: None,
        repo: Some(repo.to_path_buf()),
        acceptance_criteria: vec![],
        verification: args.verify.clone(),
        writable_scope: vec![],
        timeout_secs: args.timeout_secs,
        model: None,
        no_change_record: None,
        pane: None,
        // Every seat runs in its own worktree; the supervisor needs nothing stated for that.
        isolation: None,
        allow_concurrent_writes: None,
        profile: None,
        candidates: args.on.clone(),
        race: policy(args),
    }
}

pub fn main(argv: &[String]) -> Result<ExitCode, Exit> {
    Ok(run(&parse(argv)?).unwrap_or_else(|code| code))
}

fn run(args: &RaceArgs) -> Result<ExitCode, ExitCode> {
    let (repo, state) = super::resolve_project(&args.place).ok_or(ExitCode::FAILURE)?;
    let base_url = super::endpoint(&args.backend)?;
    let project_key = socket::project_root(&repo);
    let sock = socket::socket_paths(&state, &project_key, socket::own_uid());
    let project = marion_core::paths::ProjectDir::new(&state, &project_key);
    let launch = detach::Launch {
        program: super::supervisor_binary(),
        state_dir: state,
        project_root: project_key,
        idle_grace: super::RUN_IDLE_GRACE,
        auth: args.backend.auth(),
        base_url,
    };
    // Held for as long as this command waits, so the supervisor has a client until the race ends.
    let _held = detach::ensure_supervisor(&sock, &launch).map_err(|e| {
        eprintln!(
            "{}",
            super::supervisor_unreachable(sock.socket(), &e.to_string())
        );
        ExitCode::FAILURE
    })?;
    let spawned = courier::spawn(&sock, params(args, &repo)).map_err(|e| {
        eprintln!("marion: the race {e}");
        ExitCode::FAILURE
    })?;
    let Some(started) = spawned.race else {
        eprintln!("marion: this project's supervisor answered a race with a single agent");
        return Err(ExitCode::FAILURE);
    };
    for seat in &started.seats {
        match (&seat.agent_id, &seat.refused) {
            (Some(id), _) => eprintln!(
                "marion: seat {} {} started as {}",
                seat.seat,
                seat.candidate,
                marion_supervisor::tree::short_id(&id.0)
            ),
            (None, Some(why)) => eprintln!(
                "marion: seat {} {} did not start: {why}",
                seat.seat, seat.candidate
            ),
            (None, None) => {}
        }
    }
    if args.detach {
        eprintln!(
            "marion: race {} is running in the background; `marion ls` watches it, and its \
             scoreboard is written to races/{}.json when it is decided",
            started.race_id.0, started.race_id.0
        );
        return Ok(ExitCode::SUCCESS);
    }
    let bound = marion_supervisor::mcp::wait_bound(args.timeout_secs);
    match courier::await_race(&sock, &project, &started.race_id, bound) {
        Ok(courier::RaceDelivered::Decided(result)) => {
            println!("race {}:\n{}", result.race_id.0, result.scoreboard());
            match result.winner_row() {
                Some(w) => {
                    eprintln!(
                        "marion: seat {} won; its work is on {}",
                        w.seat,
                        w.branch.as_deref().unwrap_or("no branch")
                    );
                    Ok(ExitCode::SUCCESS)
                }
                None => {
                    eprintln!("marion: no seat won; every seat's branch is kept");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        Ok(courier::RaceDelivered::StillRunning) => {
            eprintln!(
                "marion: race {} is still running after {} s; `marion ls` watches it",
                started.race_id.0,
                bound.as_secs()
            );
            Ok(ExitCode::FAILURE)
        }
        Err(e) => {
            eprintln!("marion: {e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        std::iter::once("race")
            .chain(words.iter().copied())
            .map(String::from)
            .collect()
    }

    /// **The flags become one race**: seats from `--on`, split and repeatable, in order; checks
    /// from `--verify`; `--first` and `--prune` as the policy, and nothing stated as no policy.
    #[test]
    fn the_flags_become_one_race_with_no_caller() {
        let a = parse(&argv(&[
            "--prompt",
            "fix it",
            "--on",
            "claude, codex:gpt-5",
            "--on=opencode:openrouter:qwen",
            "--verify",
            "cargo test",
            "--first",
            "--prune",
            "--timeout",
            "600",
        ]))
        .unwrap();
        assert_eq!(a.on, ["claude", "codex:gpt-5", "opencode:openrouter:qwen"]);
        let p = params(&a, std::path::Path::new("/r"));
        assert!(p.caller.is_none() && p.repo.as_deref() == Some(std::path::Path::new("/r")));
        assert_eq!(p.candidates, a.on);
        assert_eq!(p.verification, ["cargo test"]);
        assert_eq!(p.timeout_secs, Some(600));
        let race = p.race.expect("a policy");
        assert_eq!(race.first, Some(true));
        assert_eq!(race.losers, Some(Losers::Prune));
        let plain = parse(&argv(&["--prompt", "p", "--on", "a,b", "--verify", "true"])).unwrap();
        assert_eq!(params(&plain, std::path::Path::new("/r")).race, None);
    }

    /// A race nothing can decide, or with fewer than two seats, is refused before anything starts.
    #[test]
    fn a_race_without_its_seats_or_its_checks_is_refused_by_name() {
        for (words, why) in [
            (vec!["--on", "a,b", "--verify", "true"], "--prompt"),
            (
                vec!["--prompt", "p", "--on", "a", "--verify", "true"],
                "two seats",
            ),
            (vec!["--prompt", "p", "--on", "a,b"], "--verify"),
            (
                vec!["--prompt", "p", "--on", "a,b", "--verify", "t", "x"],
                "unexpected",
            ),
        ] {
            match parse(&argv(&words)) {
                Err(Exit::Usage(e)) => assert!(e.contains(why), "{why}: {e}"),
                other => panic!("{words:?} was not refused: {other:?}"),
            }
        }
    }
}
