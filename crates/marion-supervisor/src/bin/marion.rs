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

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration as StdDuration;

use marion_core::agent_type::{DEFAULT_TIMEOUT_SECS, builtin, builtin_names};
use marion_core::paths::state_dir;
use marion_supervisor::root;

/// How long the root may take to have marion's tool list before the run is refused. Generous —
/// the measured connect is ~70 ms — because the alternative to waiting is a run that ends in
/// plain text with no error anywhere.
const MCP_READY_TIMEOUT: StdDuration = StdDuration::from_secs(30);

fn usage() -> ! {
    eprintln!(
        "usage: marion run <agent-type> --prompt <text> [--repo <path>] [--state-dir <path>]\n\
         \x20                 [--base-url <url>] [--model <name>] [--timeout <secs>]\n\
         \n\
         agent types: {}\n\
         \n\
         --timeout is the root's node-level bound. It is a per-episode `Blocked`-only budget, not\n\
         a wall-clock ceiling: marion offers a root none.",
        builtin_names().join(", ")
    );
    std::process::exit(2)
}

struct Args {
    agent_type: String,
    prompt: String,
    repo: Option<PathBuf>,
    state_dir: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    timeout_secs: Option<u64>,
}

/// Hand-rolled, because the whole surface is one subcommand and six flags.
///
/// **An unknown flag is a refusal, never a silent ignore.** A mistyped `--state-dir` that fell
/// through would write the run's state under `$HOME` and leave the operator hunting for a
/// contract that is not where they looked.
fn parse_args(argv: &[String]) -> Option<Args> {
    if argv.first().map(String::as_str) != Some("run") {
        return None;
    }
    let agent_type = argv.get(1)?.clone();
    if agent_type.starts_with('-') {
        return None;
    }
    let mut args = Args {
        agent_type,
        prompt: String::new(),
        repo: None,
        state_dir: None,
        base_url: None,
        model: None,
        timeout_secs: None,
    };
    let mut rest = argv[2..].iter();
    while let Some(flag) = rest.next() {
        let mut take = || rest.next().cloned();
        match flag.as_str() {
            "--prompt" => args.prompt = take()?,
            "--repo" => args.repo = Some(PathBuf::from(take()?)),
            "--state-dir" => args.state_dir = Some(take()?),
            "--base-url" => args.base_url = Some(take()?),
            "--model" => args.model = Some(take()?),
            "--timeout" => args.timeout_secs = Some(take()?.parse().ok()?),
            _ => return None,
        }
    }
    if args.prompt.is_empty() {
        return None;
    }
    Some(args)
}

/// §9: "`marion run --timeout`, else its agent type's `timeout_secs`, else the same 900 s".
///
/// A zero from either source would make every `Blocked` episode expire instantly, so the floor is
/// 1 s. There is no *ceiling* and nothing to refuse: a short root bound is legitimate (a root
/// whose only wait is a 60 s permission answer), and it is not comparable to a child's total-task
/// bound because the two measure different things.
fn blocked_bound_secs(explicit: Option<u64>, agent_type_secs: u64) -> u64 {
    explicit
        .or(Some(agent_type_secs))
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .max(1)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(args) = parse_args(&argv) else {
        usage()
    };

    let Some(agent_type) = builtin(&args.agent_type) else {
        eprintln!(
            "marion: unknown agent type {:?}; known: {}",
            args.agent_type,
            builtin_names().join(", ")
        );
        return ExitCode::FAILURE;
    };

    let repo = args
        .repo
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let repo = match repo.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("marion: cannot resolve repo {}: {e}", repo.display());
            return ExitCode::FAILURE;
        }
    };
    let Some(state) = state_dir(
        args.state_dir.as_deref(),
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    ) else {
        eprintln!("marion: cannot resolve a state directory (set --state-dir or $HOME)");
        return ExitCode::FAILURE;
    };
    let base_url = args
        .base_url
        .or_else(|| std::env::var("MARION_BASE_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8099/v1".into());
    let bridge = match std::env::current_exe().map(|p| p.with_file_name("marion-supervisor")) {
        Ok(p) if p.exists() => p,
        _ => PathBuf::from("marion-supervisor"),
    };

    let blocked_bound = StdDuration::from_secs(blocked_bound_secs(
        args.timeout_secs,
        agent_type.timeout.0.as_secs(),
    ));

    let spec = root::RootSpec {
        agent_type: args.agent_type.clone(),
        repo,
        state,
        base_url,
        bridge,
        // The same precedence a child's `spawn` gets (§3.1): the flag, else the agent type's own
        // `model` key. `claude` states none, so this is a no-op today and stays one until a root
        // type does — at which point the root would otherwise have been the one path that ignored
        // its own type's model.
        model: args.model.or_else(|| agent_type.model.clone()),
    };
    let node = match root::prepare(&spec) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("marion: cannot prepare the root node: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "marion: root {} ({}) in {}",
        node.agent_id.0,
        args.agent_type,
        node.agent_dir.path().display()
    );

    match root::launch(&node, &args.prompt, blocked_bound, MCP_READY_TIMEOUT) {
        Ok(outcome) => {
            for frame in &outcome.transcript {
                println!("{frame}");
            }
            if !outcome.stderr.trim().is_empty() {
                eprintln!("{}", outcome.stderr.trim());
            }
            for tool in &outcome.denied_permissions {
                eprintln!("marion: denied {tool}: the root's Blocked bound expired unanswered");
            }
            match outcome.exit_code {
                Some(0) => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        Err(e) => {
            eprintln!("marion: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn run_takes_the_agent_type_positionally_and_the_prompt_as_a_flag() {
        let a = parse_args(&argv(&["run", "claude", "--prompt", "delegate it"])).unwrap();
        assert_eq!(a.agent_type, "claude");
        assert_eq!(a.prompt, "delegate it");
        assert!(a.timeout_secs.is_none());
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
            .is_none()
        );
    }

    #[test]
    fn a_prompt_is_required_because_a_root_with_no_turn_does_nothing() {
        assert!(parse_args(&argv(&["run", "claude"])).is_none());
    }

    #[test]
    fn anything_but_run_is_not_this_binarys_business() {
        assert!(parse_args(&argv(&["doctor"])).is_none());
        assert!(parse_args(&argv(&["run", "--prompt", "p"])).is_none());
    }

    #[test]
    fn the_blocked_bound_falls_back_from_the_flag_to_the_agent_type_to_900() {
        assert_eq!(blocked_bound_secs(Some(60), 900), 60);
        assert_eq!(blocked_bound_secs(None, 900), 900);
        assert_eq!(
            blocked_bound_secs(None, 0),
            DEFAULT_TIMEOUT_SECS,
            "a zero would expire every Blocked episode instantly"
        );
        assert_eq!(blocked_bound_secs(Some(0), 900), DEFAULT_TIMEOUT_SECS);
    }
}
