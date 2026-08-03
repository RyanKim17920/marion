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
         \x20                 [--base-url <url>] [--model <name>] [--timeout <secs>] [--live]\n\
         \n\
         agent types: {}\n\
         \n\
         --live runs the node against the vendor the operator is already logged in to, instead of\n\
         marion's canned provider. It is not a credential store and seeds nothing: no code in\n\
         marion clears a child's environment, so a harness's own login is inherited anyway, and\n\
         --live is simply marion declining to overlay a base URL and a placeholder key on top of\n\
         it. It therefore costs real money and makes real model calls.\n\
         \n\
         --live with a loopback --base-url is refused rather than obeyed: that combination aims a\n\
         real credential at a fake server, which is the one mistake whose blast radius is the\n\
         operator's account rather than the run.\n\
         \n\
         --timeout is the root's node-level bound, and what it bounds follows the harness's\n\
         surfaces: on a typed control plane (claude) it is §9's per-episode `Blocked`-only budget\n\
         and not a wall-clock ceiling, since marion offers a root none; on a LaunchOnly surface\n\
         (codex, gemini, opencode) there is no `Blocked` state to budget and it is the wall-clock\n\
         bound instead — which is not optional, because opencode never exits on a provider hang.",
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
    live: bool,
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
        live: false,
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
            // The one flag with no value. Deliberately not `--live=true`: a mode this expensive
            // should read as a decision at the call site, not as a setting.
            "--live" => args.live = true,
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

/// marion's own canned provider, where a run points when nothing says otherwise.
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
/// Under `--live` the answer is normally **no endpoint at all**: each harness resolves the vendor it
/// is already logged in to, which is the whole premise — marion overlays nothing rather than
/// pointing the node somewhere. `$MARION_BASE_URL` is ignored under `--live` for that reason; it
/// names marion's canned server, and inheriting it silently is precisely the accident below.
///
/// **`--live` with a loopback `--base-url` is refused.** It is the one combination that aims a real
/// credential at a fake server: the operator's own key or OAuth token, sent in the clear to whatever
/// is listening on that port. Every other flag conflict here costs a run; this one can cost a
/// credential, so it is an error naming the cause rather than a precedence rule.
fn resolve_base_url(
    live: bool,
    explicit: Option<String>,
    from_env: Option<String>,
) -> Result<Option<String>, String> {
    match (live, explicit) {
        (true, Some(u)) if is_loopback_url(&u) => Err(format!(
            "--live with a loopback --base-url ({u}) is refused: --live makes the node present \
             the operator's own credential, and a loopback endpoint is marion's canned provider \
             or some other local process — the combination sends a real credential to a fake \
             server. Drop --base-url to let the harness reach its own vendor, or drop --live to \
             run against the canned endpoint."
        )),
        // An explicit non-loopback endpoint under --live is a proxy or a gateway, which is a
        // legitimate thing to point a real credential at.
        (true, explicit) => Ok(explicit),
        (false, explicit) => Ok(Some(
            explicit
                .or(from_env)
                .unwrap_or_else(|| CANNED_BASE_URL.into()),
        )),
    }
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
    let base_url = match resolve_base_url(
        args.live,
        args.base_url.clone(),
        std::env::var("MARION_BASE_URL").ok(),
    ) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("marion: {e}");
            return ExitCode::FAILURE;
        }
    };
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
        prompt: args.prompt.clone(),
        repo,
        state,
        base_url,
        bridge,
        // The same precedence a child's `spawn` gets (§3.1): the flag, else the agent type's own
        // `model` key. `claude` and `codex` state none and resolve to `None`, exactly as before;
        // `gemini` and `opencode` state one, which is what makes them launchable as roots at all —
        // both adapters refuse to compile without an explicit model (§6.4).
        model: args.model.or_else(|| agent_type.model.clone()),
        auth: if args.live {
            marion_harness::Auth::Inherited
        } else {
            marion_harness::Auth::Canned
        },
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

    match root::launch(&node, blocked_bound, MCP_READY_TIMEOUT) {
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
            if outcome.timed_out {
                eprintln!(
                    "marion: the root exceeded its {} s wall-clock bound and its process group \
                     was killed",
                    blocked_bound.as_secs()
                );
                return ExitCode::FAILURE;
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
    fn live_is_a_valueless_flag_and_is_off_unless_it_is_typed() {
        assert!(
            !parse_args(&argv(&["run", "claude", "--prompt", "p"]))
                .unwrap()
                .live
        );
        let a = parse_args(&argv(&["run", "claude", "--prompt", "p", "--live"])).unwrap();
        assert!(a.live);
        assert!(a.base_url.is_none());
        // It takes no value, so a following flag is still parsed as a flag rather than eaten.
        let a = parse_args(&argv(&["run", "claude", "--live", "--prompt", "p"])).unwrap();
        assert!(a.live && a.prompt == "p");
    }

    /// **The safety gate.** `--live` makes the node present the operator's own credential; a
    /// loopback endpoint is marion's canned provider or some other local process. The combination
    /// sends a real credential to a fake server, and it is the one flag conflict whose cost is the
    /// operator's account rather than the run — so it is an error, not a precedence rule.
    #[test]
    fn live_with_a_loopback_base_url_is_refused_and_the_refusal_says_why() {
        for u in [
            "http://127.0.0.1:8099/v1",
            "http://localhost:8099/v1",
            "http://[::1]:8099/v1",
            "https://127.0.0.2/v1",
            "http://127.0.0.1",
        ] {
            let e = resolve_base_url(true, Some(u.into()), None)
                .expect_err("{u} is loopback and --live must refuse it");
            assert!(e.contains(u), "the refusal must quote the URL: {e}");
            assert!(
                e.contains("real credential") && e.contains("fake server"),
                "it must name why, not merely refuse: {e}"
            );
        }
    }

    /// The gate is not a ban on `--base-url` under `--live`: a non-loopback endpoint is a proxy or a
    /// gateway, which is a legitimate thing to point a real credential at. A check that refused
    /// those too would be safe and useless.
    #[test]
    fn live_with_a_remote_base_url_is_allowed_because_a_proxy_is_a_real_endpoint() {
        assert_eq!(
            resolve_base_url(true, Some("https://gateway.example.com/v1".into()), None),
            Ok(Some("https://gateway.example.com/v1".into()))
        );
    }

    /// `--live` means *marion names no endpoint*, so each harness resolves the vendor it is already
    /// logged in to. `$MARION_BASE_URL` is ignored rather than inherited — it names marion's canned
    /// server, and inheriting it silently is exactly the accident the gate above refuses loudly.
    #[test]
    fn live_implies_no_base_url_at_all_and_ignores_the_canned_one_in_the_environment() {
        assert_eq!(resolve_base_url(true, None, None), Ok(None));
        assert_eq!(
            resolve_base_url(true, None, Some(CANNED_BASE_URL.into())),
            Ok(None),
            "an inherited canned endpoint under --live is the very mistake being prevented"
        );
    }

    /// And canned mode's precedence is untouched: the flag, then the environment, then the default.
    #[test]
    fn without_live_the_base_url_falls_back_from_the_flag_to_the_environment_to_the_canned_default()
    {
        assert_eq!(
            resolve_base_url(
                false,
                Some("http://x/v1".into()),
                Some("http://y/v1".into())
            ),
            Ok(Some("http://x/v1".into()))
        );
        assert_eq!(
            resolve_base_url(false, None, Some("http://y/v1".into())),
            Ok(Some("http://y/v1".into()))
        );
        assert_eq!(
            resolve_base_url(false, None, None),
            Ok(Some(CANNED_BASE_URL.into()))
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
