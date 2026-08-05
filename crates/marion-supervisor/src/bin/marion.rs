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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;

use marion_core::agent_type::{DEFAULT_TIMEOUT_SECS, builtin, builtin_names};
use marion_core::contract::ExitStatus;
use marion_core::paths::state_dir;
use marion_supervisor::duplex::StreamEvent;
use marion_supervisor::root;
use marion_supervisor::watch::{ChildEvent, JournalWatch};
use serde_json::Value;

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
/// **This reads marion's own shape, not a harness's**, which is why it is allowed to be specific:
/// `TaskContract` is defined in this workspace and changing it breaks this compile-adjacent
/// expectation loudly, in a test. A missing field yields `None` and the caller falls back to the
/// generic brief, so a contract that grows or loses a field degrades rather than lies.
fn contract_summary(text: &str) -> Option<String> {
    // A failed spawn puts a one-line account *above* the contract (`bridge::failure_line`), so the
    // JSON starts at the first `{` rather than at byte zero.
    let start = text.find('{')?;
    let v: Value = serde_json::from_str(text[start..].trim()).ok()?;
    let harness = v["child"]["harness"].as_str()?;
    let completion = v.get("completion")?;
    let status = completion["status"].as_str().unwrap_or("(no status)");
    let mut summary = format!("the {harness} child returned {status}");
    if let Some(head) = text[..start]
        .trim()
        .lines()
        .next()
        .filter(|l| !l.is_empty())
    {
        // The failure line marion itself wrote, kept whole: it names what went wrong.
        summary = format!("{}\n{summary}", head.trim());
    }
    match completion["narrative"]["value"].as_str() {
        Some(n) => Some(format!("{summary}: {}", brief(n, LINE_CHARS))),
        None => Some(format!("{summary}, reporting no narrative")),
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
    let frame = match event {
        // A line the node wrote that was not JSON at all is almost always a crash or a warning from
        // the harness. Verbatim, and marked as coming from the node rather than from marion.
        StreamEvent::Unparsed(line) => return say(out, "stdout", line.trim_end()),
        StreamEvent::Frame(f) => f,
    };
    let subtype = frame["subtype"].as_str().unwrap_or_default();
    match frame["type"].as_str().unwrap_or_default() {
        "assistant" => render_assistant(frame, out),
        "user" => render_user(frame, out),
        "result" => render_result(frame, out),
        "system" if subtype == "init" => {
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
        "control_request" if frame["request"]["subtype"] == "can_use_tool" => say(
            out,
            "PERMIT",
            &format!(
                "{} — marion has nobody to ask (§9); it will be denied when the Blocked bound expires",
                frame["request"]["tool_name"]
                    .as_str()
                    .unwrap_or("(unnamed tool)")
            ),
        ),
        "control_request" => say(
            out,
            "control",
            &format!(
                "{} — marion does not implement it and is answering with an error",
                frame["request"]["subtype"]
                    .as_str()
                    .unwrap_or("(no subtype)")
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
        // Rule 1. Not a dump and not silence: the kind, and its subtype when it has one.
        "" => say(out, "frame", "a stdout frame with no `type` field"),
        other => say(
            out,
            "frame",
            &match subtype {
                "" => other.to_string(),
                s => format!("{other}/{s}"),
            },
        ),
    }
}

/// One [`ChildEvent`], as the line a watcher sees.
///
/// **These are the minutes that used to be blank.** The root's own frames stop at
/// `MARION spawn …`: the child is driven inside the bridge's process, whose stdout is an MCP stream
/// and which must stay silent, so nothing about it could ever reach here directly. What reaches
/// here instead is what marion already wrote down — the journal — read back by
/// [`marion_supervisor::watch`].
fn render_child(event: &ChildEvent, out: &mut dyn Write) -> io::Result<()> {
    // A child with no intent record is a real case, not a defect: its intent may predate this
    // watch's cursor. Naming it "a child" is honest; inventing an agent type would not be.
    let name = |t: &Option<String>| t.clone().unwrap_or_else(|| "a child".into());
    match event {
        ChildEvent::Started {
            agent_type,
            harness,
            depth,
            pid,
            ..
        } => {
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
                &format!("{} started ({})", name(agent_type), facts.join(", ")),
            )
        }
        ChildEvent::Aborted {
            agent_type, reason, ..
        } => say(
            out,
            "FAILED",
            &format!("{} never started: {reason}", name(agent_type)),
        ),
        ChildEvent::Exited {
            agent_type,
            status,
            exit,
            ..
        } => {
            let verdict = match status {
                Some(s) => format!("{s:?}"),
                None => "an unrecorded status".into(),
            };
            let ok = *status == Some(ExitStatus::Ok);
            let detail = match exit {
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
            };
            say(
                out,
                if ok { "CHILD" } else { "FAILED" },
                &format!("{} exited {verdict}{detail}", name(agent_type)),
            )
        }
        ChildEvent::Denied {
            agent_type,
            tool,
            reason,
            ..
        } => say(
            out,
            "PERMIT",
            &format!("{} was denied {tool}: {reason}", name(agent_type)),
        ),
        // The viewer stopping is news in its own right — the alternative is a view that quietly
        // stops updating, which reads exactly like a run in which nothing further happened.
        ChildEvent::Stopped { reason } => say(out, "view", reason),
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

/// How often the journal is polled while a run is in flight.
///
/// **100 ms, chosen against the writer's own cadence.** Everything but §4.3's barrier records rides
/// a ~50 ms group-commit timer (`journal::Journal::tick`), so a record becomes visible to any
/// reader at a ~50 ms granularity and polling faster than that buys latency that does not exist.
/// Twice the commit interval keeps the worst-case lag around a tenth of a second — under what a
/// person reads as delay — for ten `open`+`metadata` pairs a second on a local file, and a poll
/// with no news reads zero bytes. This is a viewer: it is not worth a byte of the run's own budget.
const JOURNAL_POLL: StdDuration = StdDuration::from_millis(100);

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

    // **The live view, on stderr.** Deliberately not stdout, and checked rather than assumed:
    // marion's stdout is a machine surface with a live consumer — `tests/launch_only_root.rs` reads
    // it back with `adapter.marion_tool_calls(&run.stdout)`, and says why in as many words ("the
    // frame it emitted must survive onto marion's own stdout, still readable as the marion call it
    // was"). A root has no `TaskContract` to return (§9), so that frame stream *is* its result, and
    // prose interleaved into it would be prose in somebody's parse. stderr already carries every
    // other line marion says to a person — the banner above, the denials below, the node's own
    // stderr — so the stream joins them, and `2>/dev/null` still leaves clean frames on stdout.
    //
    // Errors are dropped rather than escalated: a closed stderr must not be what ends a run that is
    // otherwise working, and there is nowhere left to report it to anyway.
    // **Two writers, one terminal.** See [`Terminal`].
    let terminal = std::sync::Arc::new(Terminal::new(io::stderr()));

    // The journal's first production reader (§4.2, §10). Started **before** the run, from the
    // journal's current end, so this run's own children are the only news it can report.
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let poller = {
        let journal = node.project.journal();
        let root_id = node.agent_id.clone();
        let terminal = std::sync::Arc::clone(&terminal);
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut watch = JournalWatch::at_end(&journal, root_id);
            follow_journal(
                &mut watch,
                &|| stop.load(Ordering::Relaxed),
                &|| std::thread::sleep(JOURNAL_POLL),
                &mut |event| {
                    terminal.show(&|w| {
                        let _ = render_child(event, w);
                    })
                },
            );
        })
    };

    let watch = |event: StreamEvent<'_>| {
        terminal.show(&|w| {
            let _ = render_event(event, w);
        });
    };

    let launched = root::launch_watched(&node, blocked_bound, MCP_READY_TIMEOUT, Some(&watch));
    // One more poll after the run, then join. `Relaxed` is enough: the thread's own loop reads the
    // flag before its final poll, so the ordering that matters is "poll after the flag was seen",
    // which the loop enforces structurally rather than through this store.
    stop.store(true, Ordering::Relaxed);
    let _ = poller.join();

    match launched {
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
        assert!(ok[0].contains("the codex child returned Ok"), "{:?}", ok[0]);
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
            failed[1].contains("the codex child returned Failed"),
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
        assert!(ok[0].contains("codex-impl exited Ok"), "{:?}", ok[0]);
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
        assert!(failed[0].contains("exited TimedOut"), "{:?}", failed[0]);
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
                            agent_id: child.clone(),
                            parent_id: Some(root.clone()),
                            agent_type: "codex-impl".into(),
                            harness: marion_core::harness::Harness::Codex,
                            depth: 1,
                            task_id: None,
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
