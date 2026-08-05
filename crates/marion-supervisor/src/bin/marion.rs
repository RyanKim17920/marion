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
use std::time::Duration as StdDuration;

use marion_core::agent_type::{DEFAULT_TIMEOUT_SECS, builtin, builtin_names};
use marion_core::paths::state_dir;
use marion_supervisor::root;

/// How long the root may take to have marion's tool list before the run is refused. Generous —
/// the measured connect is ~70 ms — because the alternative to waiting is a run that ends in
/// plain text with no error anywhere.
const MCP_READY_TIMEOUT: StdDuration = StdDuration::from_secs(30);

fn usage_text() -> String {
    format!(
        "usage: marion                                (interactive: pick harness, model, prompt)\n\
         \x20      marion run <agent-type> --prompt <text> [--repo <path>] [--state-dir <path>]\n\
         \x20                 [--model <name>] [--timeout <secs>] [--canned [--base-url <url>]]\n\
         \n\
         agent types: {}\n\
         \n\
         marion with no arguments asks three questions — harness, model, prompt — and then runs\n\
         exactly what `marion run <answer> --prompt <answer>` would. It asks **only** when stdin\n\
         is a terminal: in a pipe or under CI there is nobody to answer, so it prints this text\n\
         and exits non-zero rather than blocking forever on a read nothing will satisfy.\n\
         \n\
         A run uses the vendor the operator is already logged in to. marion is not a credential\n\
         store and seeds nothing: no code in it clears a child's environment, so a harness's own\n\
         login is inherited, and marion simply declines to overlay a base URL and a placeholder\n\
         key on top of it. That is the default because it is what a person at a terminal means,\n\
         and it makes real model calls that cost real money.\n\
         \n\
         --canned points the node at marion's own canned provider ({CANNED_BASE_URL}) instead.\n\
         It is the fixture the test suite runs against: it costs nothing, and it answers nothing\n\
         useful. `--live` is still accepted and now does nothing — real auth is the default it\n\
         used to have to ask for.\n\
         \n\
         --base-url belongs to --canned and is refused without it. Two different reasons, one\n\
         remedy. A loopback endpoint aims a real credential at a fake server, which is the one\n\
         mistake whose blast radius is the operator's account rather than the run. Any other\n\
         endpoint -- a proxy or a gateway -- is refused because marion does not implement it: no\n\
         adapter passes an endpoint to a node running on the operator's own login, so the run\n\
         would reach the vendor directly while looking like it went through the gateway. Pointing\n\
         a real credential through a proxy is a reasonable thing to want and may land later;\n\
         marion will not pretend to do it until it does. Pass --canned if the fixture is what was\n\
         meant.\n\
         \n\
         --repo defaults to the enclosing git repository — the nearest ancestor of the working\n\
         directory holding a `.git` — and to the working directory itself when there is none, so\n\
         that running marion from a subdirectory still scopes the node to the whole checkout.\n\
         \n\
         --state-dir defaults to $XDG_STATE_HOME/marion, else ~/.local/state/marion. It is\n\
         deliberately not repo-local and not a temp directory: §4.3's tree is what survives a run,\n\
         and a run whose journal vanished with /tmp would have nothing to resume from.\n\
         \n\
         --timeout is the root's node-level bound, and what it bounds follows the harness's\n\
         surfaces: on a typed control plane (claude) it is §9's per-episode `Blocked`-only budget\n\
         and not a wall-clock ceiling, since marion offers a root none; on a LaunchOnly surface\n\
         (codex, gemini, opencode) there is no `Blocked` state to budget and it is the wall-clock\n\
         bound instead — which is not optional, because opencode never exits on a provider hang.",
        builtin_names().join(", ")
    )
}

fn usage() -> ! {
    eprintln!("{}", usage_text());
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
    /// Opt **in** to marion's canned provider. The inverse of the flag this replaced: real auth
    /// is what a person at a terminal means, and the canned server is a test fixture.
    canned: bool,
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
        canned: false,
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
            // The one flag with no value. Deliberately not `--canned=true`: which provider a run
            // talks to should read as a decision at the call site, not as a setting.
            "--canned" => args.canned = true,
            // Accepted and inert. It used to select real auth, which is now the default; every
            // script and note already carrying it keeps working, and refusing it would break
            // them to say nothing the run does not already do.
            "--live" => {}
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
/// `background` and `verification` in `spawn::SpawnError`. Honouring a gateway is a real feature —
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

/// What the three questions produce. Everything else keeps its default: the picker exists to make
/// the common run typable, not to grow a second copy of the flag surface.
#[derive(Debug, PartialEq, Eq)]
struct Chosen {
    agent_type: String,
    /// `None` means "whatever the agent type itself defaults to", which is the same precedence
    /// `--model`'s absence gets. An empty answer is that absence, not an empty model id.
    model: Option<String>,
    prompt: String,
}

/// One question. `Ok(None)` is EOF — Ctrl-D at any prompt ends the session, it does not loop.
fn ask(
    input: &mut impl BufRead,
    out: &mut impl Write,
    question: &str,
) -> io::Result<Option<String>> {
    write!(out, "{question}")?;
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        writeln!(out)?;
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

/// The interactive picker, over any reader and writer so its logic is testable without a terminal.
///
/// The harness list comes from `builtin_names()` rather than a literal, so a fifth built-in is
/// offered the day it is added and cannot be forgotten here. `Ok(None)` is EOF at any question.
fn pick(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Option<Chosen>> {
    let names = builtin_names();
    writeln!(out, "marion — pick a harness:")?;
    for (i, name) in names.iter().enumerate() {
        let desc = builtin(name).map_or(String::new(), |t| format!("  {}", t.description));
        writeln!(out, "  {}) {name}{desc}", i + 1)?;
    }
    let agent_type = loop {
        let Some(answer) = ask(input, out, "harness [1]: ")? else {
            return Ok(None);
        };
        // Empty takes the first, which is the root orchestrator — the one a bare `marion` means.
        if answer.is_empty() {
            break names[0].to_string();
        }
        if let Some(n) = answer
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=names.len()).contains(n))
        {
            break names[n - 1].to_string();
        }
        // A name typed out is the same answer as its number; refusing it would be pedantry.
        if let Some(name) = names.iter().find(|n| **n == answer) {
            break (*name).to_string();
        }
        writeln!(out, "  not one of 1..={}, nor a listed name.", names.len())?;
    };

    let default_model = builtin(&agent_type).and_then(|t| t.model);
    let model_hint = default_model
        .clone()
        .unwrap_or_else(|| "the harness's own default".into());
    // Free text, not a menu: marion has no model catalogue and inventing one would go stale the
    // week a vendor ships an id it does not list.
    let Some(model) = ask(input, out, &format!("model [{model_hint}]: "))? else {
        return Ok(None);
    };
    let model = if model.is_empty() {
        default_model
    } else {
        Some(model)
    };

    let prompt = loop {
        let Some(answer) = ask(input, out, "prompt: ")? else {
            return Ok(None);
        };
        if !answer.is_empty() {
            break answer;
        }
        // A root with no turn does nothing, so an empty answer re-asks rather than launching a
        // run whose only outcome is a wasted process.
        writeln!(out, "  a run needs a prompt.")?;
    };

    Ok(Some(Chosen {
        agent_type,
        model,
        prompt,
    }))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage_text());
        return ExitCode::SUCCESS;
    }
    let args = if argv.is_empty() {
        // **Only** with a terminal on the other end. A picker that read from a pipe would block
        // forever on input nothing is going to send, which is a hang, not a prompt.
        if !io::stdin().is_terminal() {
            usage()
        }
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut out = io::stderr();
        match pick(&mut input, &mut out) {
            Ok(Some(chosen)) => Args {
                agent_type: chosen.agent_type,
                prompt: chosen.prompt,
                repo: None,
                state_dir: None,
                base_url: None,
                model: chosen.model,
                timeout_secs: None,
                canned: false,
            },
            // EOF: the operator changed their mind, which is not an error.
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("marion: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        match parse_args(&argv) {
            Some(a) => a,
            None => usage(),
        }
    };

    let Some(agent_type) = builtin(&args.agent_type) else {
        eprintln!(
            "marion: unknown agent type {:?}; known: {}",
            args.agent_type,
            builtin_names().join(", ")
        );
        return ExitCode::FAILURE;
    };

    let repo = args.repo.unwrap_or_else(|| {
        default_repo(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    });
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
        args.canned,
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
        auth: if args.canned {
            marion_harness::Auth::Canned
        } else {
            marion_harness::Auth::Inherited
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

    /// The canned provider is a **fixture**, so reaching it is the thing that must be typed. A
    /// person running marion means their own logged-in harness, and a default that silently aimed
    /// them at a fake server on 127.0.0.1 answered a question nobody asked.
    #[test]
    fn canned_is_a_valueless_flag_and_real_auth_is_what_a_bare_run_gets() {
        assert!(
            !parse_args(&argv(&["run", "claude", "--prompt", "p"]))
                .unwrap()
                .canned
        );
        let a = parse_args(&argv(&["run", "claude", "--prompt", "p", "--canned"])).unwrap();
        assert!(a.canned);
        assert!(a.base_url.is_none());
        // It takes no value, so a following flag is still parsed as a flag rather than eaten.
        let a = parse_args(&argv(&["run", "claude", "--canned", "--prompt", "p"])).unwrap();
        assert!(a.canned && a.prompt == "p");
    }

    /// `--live` used to select real auth. Real auth is now the default, so the flag says nothing
    /// the run does not already do — but every script and note carrying it must keep working, so
    /// it is accepted and inert rather than a refusal.
    #[test]
    fn live_is_still_accepted_and_now_means_exactly_nothing() {
        let a = parse_args(&argv(&["run", "claude", "--prompt", "p", "--live"])).unwrap();
        assert!(!a.canned, "--live is the default, not the canned provider");
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
    /// The refusal is the reversible direction (`spawn::SpawnError`'s `background` and
    /// `verification` are the precedent): honouring can land later, but an operator taught that
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

    fn run_picker(input: &str) -> (Option<Chosen>, String) {
        let mut reader = io::Cursor::new(input.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        let chosen = pick(&mut reader, &mut out).expect("a Cursor cannot fail to read");
        (chosen, String::from_utf8(out).expect("prompts are utf-8"))
    }

    #[test]
    fn the_picker_offers_every_builtin_by_number_and_selects_by_it() {
        // Looked up rather than hardcoded, for the reason the test below states: the offered list
        // *is* `builtin_names()`, so an ordinal written as a literal here would silently start
        // naming a different type the day a built-in is added — which is exactly what adding
        // `claude-impl` and `gemini-impl` did to the literal `4` that used to sit here.
        let n = builtin_names()
            .iter()
            .position(|n| *n == "gemini")
            .expect("gemini is a built-in")
            + 1;
        let (chosen, shown) = run_picker(&format!("{n}\ngemini-9.9-pro\nport the parser\n"));
        for name in builtin_names() {
            assert!(shown.contains(name), "{name} must be offered: {shown}");
        }
        assert_eq!(
            chosen,
            Some(Chosen {
                agent_type: "gemini".into(),
                model: Some("gemini-9.9-pro".into()),
                prompt: "port the parser".into(),
            })
        );
    }

    /// The list is `builtin_names()`, never a literal, so a fifth built-in is offered the day it
    /// exists. Selecting the last one by its number is what proves the two are the same list.
    #[test]
    fn the_last_offered_number_is_the_last_builtin_whatever_that_becomes() {
        let n = builtin_names().len();
        let (chosen, _) = run_picker(&format!("{n}\n\nship it\n"));
        assert_eq!(chosen.unwrap().agent_type, *builtin_names().last().unwrap());
    }

    #[test]
    fn a_harness_may_also_be_typed_by_name_and_a_bad_answer_re_asks() {
        let (chosen, shown) = run_picker("nope\n99\n0\nopencode\n\nship it\n");
        assert_eq!(chosen.as_ref().unwrap().agent_type, "opencode");
        assert_eq!(
            shown.matches("not one of").count(),
            3,
            "each bad answer is told so and re-asked, not silently taken: {shown}"
        );
    }

    /// An empty model answer is the *absence* of `--model`, which is what makes the agent type's
    /// own default apply — including for the two harnesses whose adapters refuse without one.
    #[test]
    fn an_empty_model_answer_takes_the_agent_types_own_default() {
        let (chosen, shown) = run_picker("opencode\n\nship it\n");
        let c = chosen.unwrap();
        assert_eq!(c.model, builtin("opencode").unwrap().model);
        assert!(
            shown.contains(&builtin("opencode").unwrap().model.unwrap()),
            "the default is shown, so an empty answer is an informed one: {shown}"
        );

        // And where the type states none, an empty answer stays none rather than becoming "".
        let (chosen, shown) = run_picker("claude\n\nship it\n");
        assert_eq!(chosen.unwrap().model, None);
        assert!(shown.contains("the harness's own default"), "{shown}");
    }

    #[test]
    fn a_prompt_is_free_text_and_keeps_its_spaces() {
        let (chosen, _) = run_picker("1\n\n  delegate the parser rewrite  \n");
        assert_eq!(chosen.unwrap().prompt, "delegate the parser rewrite");
    }

    /// An empty prompt re-asks rather than launching: a root with no turn does nothing, and
    /// spending a process to discover that is the opposite of helpful.
    #[test]
    fn an_empty_prompt_re_asks_rather_than_launching_an_empty_run() {
        let (chosen, shown) = run_picker("1\n\n\n   \nfinally\n");
        assert_eq!(chosen.unwrap().prompt, "finally");
        assert_eq!(shown.matches("a run needs a prompt").count(), 2);
    }

    /// Ctrl-D at any question ends the session. Not a panic, and — the failure mode that matters —
    /// not a loop that re-asks a closed stdin forever.
    #[test]
    fn eof_at_any_question_ends_the_picker_cleanly() {
        for input in ["", "1\n", "1\n\n", "1\nsonnet\n", "nope\n"] {
            let (chosen, _) = run_picker(input);
            assert_eq!(chosen, None, "EOF after {input:?} must end it");
        }
    }

    /// The usage text is the only place the flag semantics are stated to a person, so it has to
    /// track them. A stale line here is the same bug as a stale default.
    #[test]
    fn the_usage_text_describes_the_flags_that_exist() {
        let u = usage_text();
        assert!(u.contains("--canned"), "the flag that reaches the fixture");
        assert!(u.contains(CANNED_BASE_URL), "and where that points");
        assert!(u.contains("`--live` is still accepted"), "{u}");
        // The stale-promise check. The text used to advertise `--base-url` under real auth as a
        // legitimate way to reach a proxy, which marion refuses as unimplemented — a promise in
        // `--help` is the same class of bug as a stale default, one layer out.
        assert!(
            u.contains("--base-url belongs to --canned and is refused without it"),
            "the usage text must not promise an endpoint under real auth that marion refuses: {u}"
        );
        assert!(
            u.contains("marion does not implement it"),
            "and it must say which of the two refusals is a capability limit: {u}"
        );
        for name in builtin_names() {
            assert!(u.contains(name), "{name} must be listed");
        }
        assert!(u.contains("terminal"), "the non-TTY guard is documented");
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
