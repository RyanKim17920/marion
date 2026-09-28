//! §6.1 step 8 for the fifth harness: driving a spawned ACP agent through one real turn.
//!
//! marion can put a node on four harnesses. The fifth — §5.2's `acp` row — is compiled all the way
//! to an [`Invocation`] and an MCP declaration by `AcpAdapter`, and then has nowhere to go:
//! `duplex::launch_path` maps every `ControlTransport::Typed(_)` to `LaunchPath::Duplex`, and
//! `duplex::run_duplex` speaks claude-code's `stream-json`. An ACP agent handed a `stream-json`
//! frame answers nothing, so the declaration that S21 proved carries marion's bridge has never
//! reached a node. This module is the missing driver.
//!
//! # What it is, and what it is a second copy of
//!
//! The mechanism is `doctor`'s: `AcpChild`/`acp_live_turn` already take a real `opencode acp` from
//! spawn to `end_turn` from the shipped `marion-supervisor doctor` binary, and nothing here is a
//! new idea about ACP. What differs is what the two are *for*, and the differences are not
//! cosmetic:
//!
//! * **doctor probes; this runs a node.** doctor's `session/new` deliberately carries
//!   [`McpDeclaration::None`] so marion is not on both ends of its own assertion (§8). A node run
//!   carries the adapter's real declaration — the whole point of the fifth `McpRoute` — so the id
//!   this driver correlates its answer against is read *out of the declaration the adapter built*
//!   rather than assumed, and a declaration that is neither a `session/new` nor a `session/load`
//!   request is refused before anything is spawned. A `session/load` is a **resume** — ACP's own,
//!   a request rather than a flag — and goes out only to an agent whose `initialize` advertised
//!   `loadSession`; the prompt then continues the session the load named.
//! * **doctor answers nothing the agent asks; a node run must.** doctor's micro-prompt is chosen so
//!   the turn never needs a permission or a file, and `AcpChild` consequently has no inbound
//!   request path at all. A real prompt does need them — S21's own probe grew
//!   `answer_agent_requests` for exactly this — and an unanswered client-bound request hangs the
//!   turn until the wall clock kills it. Worse, the shipped [`acp::initialize_request`] advertises
//!   `fs.readTextFile`, `fs.writeTextFile` and `terminal`, so marion has already told the agent it
//!   will answer three kinds of request before this driver sees the first one.
//! * **doctor reports prose; this returns a transcript.** The three fields every other child path
//!   returns, with `stdout` being the agent's own frames verbatim so `acp::parse_stream` and
//!   `acp::marion_calls` read what the agent wrote and not a re-serialisation of it.
//!
//! # The transcript is the agent's stdout and nothing else
//!
//! `tests/fixtures/s21/opencode-acp-session.jsonl` is one direction only — every line in it is a
//! frame `opencode acp` wrote — and `acp::json_frames` is a line splitter with no notion of
//! direction. Interleaving marion's own requests into `stdout` here would put frames carrying
//! `"method":"session/prompt"` in front of a reader that scans every frame for `result.stopReason`
//! and for `session/update` tool calls. It would not currently misread one, which is precisely why
//! it must not be done on the strength of that: the reader is written against a capture of one
//! direction, so this produces one direction.
//!
//! # Threads, not a runtime
//!
//! The workspace has no async runtime and `agent-client-protocol` 2.0.0 is not a dependency. One
//! detached reader thread per pipe and a poll loop, exactly as `doctor` and `run_bounded` do it.

use std::io::{BufRead, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use marion_harness::{AgentHandshake, ChildExit, Invocation, acp};

use crate::kill::{DRAIN_GRACE, kill_process_tree};
use crate::run::Drain;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;

/// The JSON-RPC id marion stamps on `initialize`. doctor's number, kept, so a capture taken with
/// one and read against the other still correlates.
const INITIALIZE_ID: u64 = 0;
/// The id on the first `session/prompt`; each later prompt — a message from the node's inbox — takes
/// the next. `session/new`'s is **not** a constant here — it is read out of the declaration the
/// adapter compiled (`marion_harness::adapter::SESSION_NEW_ID` is where it is stamped), because a
/// driver that assumed the value and an adapter that changed it would not fail loudly; they would
/// wait out the whole session budget for an answer that had already arrived under a different id.
const FIRST_PROMPT_ID: u64 = 2;
/// The id on the request that sets a session select (the model, then the approval mode), one at a
/// time. The inbox's second prompt reuses the number: every select is answered and its answer
/// consumed before the first prompt is sent, so the two never share a pending id.
const SET_SELECT_ID: u64 = 3;
/// How long the agent is given to answer one select. A local state change on every agent measured,
/// answered with no network round trip; bounded like every other step.
const SET_SELECT_BUDGET: Duration = Duration::from_secs(30);

/// `MessageDelivered.via` for a prompt sent while an earlier one was still running, and for one
/// sent after every earlier one settled.
pub const VIA_MID_TURN: &str = "acp:mid-turn";
pub const VIA_NEXT_TURN: &str = "acp:next-turn";

/// How long the agent is given to answer `initialize`. A process start and one frame.
///
/// S22 measured what makes this a budget and not a formality: `npx -y @agentclientprotocol/codex-acp`
/// was still *downloading* when 30 s expired, returned zero frames, and looked exactly like a dead
/// agent. The same version run from `node_modules/.bin` handshakes in under a second. So an expiry
/// here is reported as an expiry with the agent's stderr attached, never as "the agent is broken".
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(30);

/// How long `session/new` is given. Longer than the handshake because S20 measured it reaching a
/// vendor over the network before answering — including to say no (`gemini --acp`'s `-32000`).
const SESSION_BUDGET: Duration = Duration::from_secs(60);

/// How long the cancelled turn is given to answer after `session/cancel`.
///
/// `session/cancel` is a notification: the pending `session/prompt` answers it with
/// `stopReason: "cancelled"` (§5.2). That answer is worth waiting a few seconds for — it is the
/// agent's own statement that it stopped, and it lands in the transcript where a reader can see it
/// — but it is not worth waiting for indefinitely, because an agent that ignores the cancel is
/// exactly the agent the kill below exists for.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// How long the agent is given to go away on SIGINT before the process group is killed. doctor's
/// number, for the same reason: §8 calls a binary that hangs on interrupt a distinct finding from a
/// binary that is absent.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// How often a wait wakes to re-read the frame sink.
const POLL: Duration = Duration::from_millis(20);

/// How long after the agent's process is gone marion keeps reading its pipe before concluding the
/// answer is never coming. Not zero: the frames are drained by another thread, so an agent that
/// wrote its last frame and exited in the same breath has a window in which the process is dead and
/// the answer is still in flight through a pipe and a mutex.
const DEATH_GRACE: Duration = Duration::from_millis(250);

/// How much of the agent's stderr a refusal carries. Enough to hold a stack trace's last words;
/// bounded because an error message is read by a person and an agent that fails at startup can
/// write megabytes.
const STDERR_EXCERPT: usize = 2000;

/// What a driven ACP turn leaves behind, in the three fields every marion child path returns.
#[derive(Debug)]
pub struct AcpRun {
    /// Every frame the agent wrote, one per line, verbatim and in arrival order — including lines
    /// that are not JSON, because `acp::parse_stream` is given what the agent wrote and decides for
    /// itself. This is the string the adapter's reader consumes.
    pub stdout: String,
    pub stderr: String,
    /// **Deliberately not the turn's verdict.** `acp::parse_stream` ignores it, and this driver is
    /// the reason it can: an ACP agent is a long-lived stdio server that marion kills, so this
    /// describes marion's shutdown. What describes the turn is `stopReason` and the frames.
    pub exit: ChildExit,
    /// A pipe was still open when the drain bound expired, so the capture is a prefix (§6.7:
    /// recorded, never silent).
    pub capture_truncated: bool,
}

/// One ACP turn, as plain data.
pub struct AcpChildSpec<'a> {
    /// Program, args, env and cwd, already compiled by `AcpAdapter::compile`.
    pub inv: &'a Invocation,
    /// The `session/new` **request** the adapter built (`HarnessAdapter::session_declaration`),
    /// whole — envelope, id and all — or `None` for a session with no MCP servers declared.
    ///
    /// The whole request rather than its params, because the id is half of what a driver needs and
    /// splitting the frame here would mean re-assembling it against a second idea of what the
    /// adapter stamped. `None` compiles `acp::session_new_request(_, cwd, &[])`: an **empty**
    /// `mcpServers`, which is the shape S21 sent on every probe, and never an absent key.
    pub session_declaration: Option<Value>,
    pub prompt: &'a str,
    /// The wall clock for the turn.
    ///
    /// It bounds the whole conversation, not each step: `initialize` and `session/new` take their
    /// own measured budgets clipped to whatever is left of this, and the prompt gets the remainder.
    /// The function returns within `bound` plus the shutdown grace (a cancel, an interrupt and a
    /// drain), which is bounded and small — never within `bound` exactly, because a kill that is
    /// not waited on is a leak.
    pub bound: Duration,
    /// Called with the pid the instant it is known, **before one byte is written to the node**.
    ///
    /// §6.1 step 7's confirmation is the caller's to write and this is the first instant it can be
    /// written truthfully. See the same call in `duplex::run_duplex`: putting it here rather than
    /// after the handshake means the window in which a process exists and no durable record names
    /// it is one append and one fsync wide, instead of the node's whole first turn.
    pub on_started: &'a dyn Fn(i32),
    /// Called with every line the agent writes, verbatim and in arrival order, **while the turn
    /// runs** — the live seam a watched node needs, since the transcript arrives only at the end.
    /// Each line is delivered exactly once, and the concatenation is [`AcpRun::stdout`]. `None` for
    /// a node nobody watches live.
    pub on_line: Option<&'a dyn Fn(&str)>,
    /// **The node's inbox**, for every prompt after the first, or `None` for a session that takes
    /// only [`Self::prompt`]. With a feed the session ends only when every prompt has settled and
    /// a `take_or_seal` seals the inbox; see [`prompt_session`].
    pub turns: Option<crate::inbox::TurnFeed>,
}

impl std::fmt::Debug for AcpChildSpec<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpChildSpec")
            .field("inv", &self.inv)
            .field("session_declaration", &self.session_declaration)
            .field("prompt", &self.prompt)
            .field("bound", &self.bound)
            .field("on_started", &"<on_started>")
            .finish()
    }
}

/// Why marion has no transcript.
///
/// Every variant names what marion asked for and did not get, and carries the agent's own words
/// where it has any — its stderr, or the JSON-RPC error it answered with. A turn that *ran* and
/// went badly is not in here: a refused `session/prompt`, a cancelled turn and a tool call the
/// agent marked failed are all facts in the transcript, and `acp::parse_stream` is what reads them.
/// This enum is only for the cases where there is nothing to read.
#[derive(Debug, thiserror::Error)]
pub enum AcpChildError {
    #[error("could not start the ACP agent `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    /// The pipe to the agent closed under marion's own write. Distinct from a silent agent: this
    /// one says the request never went out, so no budget below it was ever really spent.
    #[error("the agent's stdin closed while marion was writing `{step}`: {source}")]
    Unwritable {
        step: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// The adapter handed over something that is not a `session/new` request. Checked before the
    /// agent is spawned, because the alternative is a live agent waiting on a frame it will never
    /// understand and a driver waiting on an id nobody stamped.
    #[error(
        "the session declaration marion compiled is not a `session/new` or `session/load` request: \
         {what}. `AcpAdapter::session_declaration` builds it with `acp::session_new_request` or \
         `acp::session_load_request`, and this driver correlates the agent's answer against the id \
         that request carries"
    )]
    Declaration { what: String },
    #[error(
        "the agent never answered `initialize` within {waited:?}, so marion never learned what it \
         is or what it can do and no session was opened. It wrote {frames} frames. Its stderr: \
         {stderr}"
    )]
    NoHandshake {
        waited: Duration,
        frames: usize,
        stderr: String,
    },
    /// It answered, and the answer is not one marion can key on — a wire version it does not speak,
    /// an anonymous agent, or a JSON-RPC error. `AcpError` already carries the agent's own words.
    #[error("the agent answered `initialize` with something marion cannot use: {0}")]
    Handshake(#[source] acp::AcpError),
    #[error(
        "the agent completed `initialize` and then never answered `session/new` within {waited:?}, \
         so there is no session to prompt. It wrote {frames} frames. Its stderr: {stderr}"
    )]
    NoSession {
        waited: Duration,
        frames: usize,
        stderr: String,
    },
    /// **The S20 blocker's shape**, and it is the agent's refusal rather than marion's failure to
    /// understand: `gemini --acp` answers `-32000`, *"This client is no longer supported for Gemini
    /// Code Assist for individuals"*. No adapter can route around it, so it is reported in the
    /// vendor's own sentence.
    #[error("the agent refused to open a session: {0}")]
    SessionRefused(#[source] acp::AcpError),
    /// The launch asked to continue a session and the agent's own `initialize` did not advertise
    /// `loadSession`, which is the only resume ACP v1 has. Refused before `session/load` is sent,
    /// by the name of the capability and of the agent, because the alternative — a `session/new`
    /// under the old id — would be a fresh run the contract misdescribes as a continuation.
    #[error(
        "the launch resumes session `{session}` and the agent (`{agent}`) does not advertise \
         `loadSession`, the one resume ACP v1 has; marion will not open a fresh session and call \
         it a continuation"
    )]
    NoLoadSession { agent: String, session: String },
    /// A session-level select was asked for — a model, or the agent type's `approval_mode` — and
    /// the session's answer advertised none of that category (`configOptions`, or `modes` for a
    /// mode), which is the only way ACP chooses one. Refused before the prompt, because a turn on
    /// the agent's own default would run under a contract naming the request.
    #[error(
        "the launch asks for {category} `{value}` and the agent (`{agent}`) advertises no \
         {category} select in its session (`configOptions` of category `{category}`), the one \
         channel ACP has for it; marion will not run the turn on a {category} nobody chose. Leave \
         it unset to run on the agent's own default"
    )]
    NoSelect {
        agent: String,
        category: &'static str,
        value: String,
    },
    /// The session offers a select of that category, and not this value.
    #[error(
        "the launch asks for {category} `{value}` and the agent (`{agent}`) does not offer it; it \
         offers: {offered}"
    )]
    NotOffered {
        agent: String,
        category: &'static str,
        value: String,
        offered: String,
    },
    /// The agent refused the set request, answered it with the select still on another value, or
    /// did not answer.
    #[error("the agent (`{agent}`) did not switch its session's {category} to `{value}`: {why}")]
    NotSet {
        agent: String,
        category: &'static str,
        value: String,
        why: String,
    },
}

/// **The turn's end, not the process's**, as a node's recorded exit: code 0 for a turn the agent
/// answered, and no code at all for one marion cut short on its bound. An ACP agent is a stdio
/// server marion shuts down once the turn settles — EOF, then SIGINT ([`AcpRun::exit`]) — so the
/// process status describes marion's shutdown, and recorded as the node's exit it would read as
/// a node killed by a signal on every ordinary run (a resumed ACP child's `marion resume` failed
/// on exactly that). What can still fail the turn is its stream, read separately. One rule for
/// roots and children alike.
pub fn turn_exit(exit: ChildExit) -> ChildExit {
    ChildExit {
        code: (!exit.timed_out).then_some(0),
        signal: None,
        timed_out: exit.timed_out,
    }
}

/// Drive one ACP turn and hand back the transcript.
///
/// The order is ACP's, and each step is bounded and refuses by name rather than falling through to
/// the next:
///
/// 1. spawn, in **its own process group** — the group is what makes the kill able to reach marion's
///    own MCP bridge, which the *agent* starts from the `session/new` declaration and which
///    inherits the agent's stdout write end;
/// 2. [`AcpChildSpec::on_started`], before one byte goes out;
/// 3. `initialize`, and a handshake marion can key on (§3.3's stage two);
/// 4. `session/new`, carrying the adapter's declaration, correlated on that request's own id;
/// 5. `session/prompt`, answering the agent's client-bound requests as they arrive;
/// 6. on expiry, `session/cancel` and then the kill — with the frames collected so far kept, and an
///    exit that says marion ended it.
pub fn run_acp_child(spec: AcpChildSpec<'_>) -> Result<AcpRun, AcpChildError> {
    // Before the spawn: a declaration marion cannot correlate against is a hang, and a hang after a
    // process exists costs the whole wall clock to discover.
    let session_new = match spec.session_declaration {
        Some(d) => d,
        None => {
            acp::session_new_request(marion_harness::adapter::SESSION_NEW_ID, &spec.inv.cwd, &[])
        }
    };
    let opening = declared(&session_new)?;

    // `checked_add`, for `run_bounded`'s reason: `Instant + Duration` panics on overflow, and every
    // escape from here on abandons a live process. Saturating to the bound's own ceiling keeps an
    // unrepresentable request finite instead of turning it into "kill it now" or "never".
    let deadline = Instant::now()
        .checked_add(spec.bound)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));

    let mut agent = Driver::spawn(spec.inv, spec.on_line)?;
    (spec.on_started)(agent.pid);

    let handshake = handshake_with(&mut agent, deadline)?;
    // The one capability the handshake gates here: a resume goes out only to an agent that says
    // it can replay a session. Checked before the frame is sent, so an agent without it never sees
    // a `session/load` it would answer with an error marion would then have to interpret.
    if let Some(session) = &opening.session
        && !handshake.load_session
    {
        let _ = agent.finish(true);
        return Err(AcpChildError::NoLoadSession {
            agent: handshake.key(),
            session: session.clone(),
        });
    }

    let (session, opened) = open_session(&mut agent, &session_new, &opening, deadline)?;
    // The model first, then the approval mode: each is its own select, and a refusal of either
    // ends the run before the prompt.
    for (category, value) in [
        (acp::MODEL_CATEGORY, spec.inv.model.as_deref()),
        (acp::MODE_CATEGORY, spec.inv.session_mode.as_deref()),
    ] {
        if let Some(value) = value {
            select_in_session(
                &mut agent,
                &handshake.key(),
                &session,
                &opened,
                category,
                value,
                deadline,
            )?;
        }
    }
    let answered = prompt_session(
        &mut agent,
        &session,
        spec.prompt,
        deadline,
        spec.turns.as_ref(),
    )?;

    let (end, _) = agent.finish(!answered);
    Ok(AcpRun {
        stdout: end.stdout,
        stderr: end.stderr,
        exit: end.exit,
        capture_truncated: end.capture_truncated,
    })
}

/// §3.3's stage two: `initialize`, and the narrowed capabilities it answers with.
///
/// Parsed, not merely received. The handshake is what narrows this agent's capabilities, and an
/// agent that answers with a wire version marion does not speak has to be refused here rather than
/// prompted anyway and read with a v1 reader.
fn handshake_with(
    agent: &mut Driver<'_>,
    deadline: Instant,
) -> Result<AgentHandshake, AcpChildError> {
    agent.send(&acp::initialize_request(INITIALIZE_ID), "initialize")?;
    let handshake = match agent.settle(INITIALIZE_ID, clip(deadline, HANDSHAKE_BUDGET)) {
        Some(f) => f,
        None => {
            return Err(agent.refuse(|end, frames| AcpChildError::NoHandshake {
                waited: HANDSHAKE_BUDGET,
                frames,
                stderr: excerpt(&end.stderr),
            }));
        }
    };
    match AgentHandshake::parse(&handshake) {
        Ok(h) => Ok(h),
        Err(e) => {
            let _ = agent.finish(true);
            Err(AcpChildError::Handshake(e))
        }
    }
}

/// The adapter's own opening request, and the session id the agent is then working in.
///
/// A fresh session's id is the agent's answer; a loaded session's id was the request's, and the
/// answer only says whether the agent took it (ACP's `session/load` result carries no id). The
/// answer itself comes back too: its `configOptions` are what [`select_in_session`] reads.
fn open_session(
    agent: &mut Driver<'_>,
    session_new: &Value,
    opening: &Opening,
    deadline: Instant,
) -> Result<(String, String), AcpChildError> {
    agent.send(session_new, opening.method)?;
    let opened = match agent.settle(opening.id, clip(deadline, SESSION_BUDGET)) {
        Some(f) => f,
        None => {
            return Err(agent.refuse(|end, frames| AcpChildError::NoSession {
                waited: SESSION_BUDGET,
                frames,
                stderr: excerpt(&end.stderr),
            }));
        }
    };
    let session = match &opening.session {
        None => acp::session_id(&opened),
        Some(s) => acp::session_loaded(&opened).map(|()| s.clone()),
    };
    match session {
        Ok(s) => Ok((s, opened)),
        Err(e) => {
            let _ = agent.finish(true);
            Err(AcpChildError::SessionRefused(e))
        }
    }
}

/// Put the session on the requested `value` of a select `category` — the model, or the approval
/// mode — through the protocol, or refuse the run by name.
///
/// ACP chooses both inside the session: the answer that opened it advertises a select per
/// category (`model` on S21 opencode and both S22 Registry shims; `mode` on all four agents probed
/// 2026-09-22), and `session/set_config_option` changes it (measured on `opencode acp` 1.18.32,
/// claude-agent-acp 0.66.0, codex-acp 1.13.0 and copilot 1.0.83 `--acp`: each answers with the
/// updated `configOptions`, and refuses a value it lacks with a JSON-RPC error).
/// [`acp::session_select`] falls back to `modes` + `session/set_mode` for a mode where no config
/// option carries it. Keyed on the category, never on an agent's name, so any agent that
/// advertises a select gets it set. Every other outcome is a refusal before the prompt, because
/// the contract records the request and a turn on anything else would make it false.
fn select_in_session(
    agent: &mut Driver,
    agent_key: &str,
    session: &str,
    opened: &str,
    category: &'static str,
    value: &str,
    deadline: Instant,
) -> Result<(), AcpChildError> {
    let refuse = |agent: &mut Driver, e: AcpChildError| {
        let _ = agent.finish(true);
        Err(e)
    };
    let Some(select) = acp::session_select(opened, category) else {
        return refuse(
            agent,
            AcpChildError::NoSelect {
                agent: agent_key.to_string(),
                category,
                value: value.to_string(),
            },
        );
    };
    if select.current.as_deref() == Some(value) {
        return Ok(());
    }
    if !select.values.iter().any(|v| v == value) {
        return refuse(
            agent,
            AcpChildError::NotOffered {
                agent: agent_key.to_string(),
                category,
                value: value.to_string(),
                offered: select.values.join(", "),
            },
        );
    }
    let method = match select.channel {
        acp::SelectChannel::ConfigOption { .. } => acp::SET_CONFIG_OPTION_METHOD,
        acp::SelectChannel::SetMode => acp::SET_MODE_METHOD,
    };
    agent.send(
        &acp::select_request(SET_SELECT_ID, session, &select, value),
        method,
    )?;
    let not_set = |why: String| AcpChildError::NotSet {
        agent: agent_key.to_string(),
        category,
        value: value.to_string(),
        why,
    };
    let Some(answer) = agent.settle(SET_SELECT_ID, clip(deadline, SET_SELECT_BUDGET)) else {
        return refuse(
            agent,
            not_set(format!("no answer within {SET_SELECT_BUDGET:?}")),
        );
    };
    // Consumed: the next select reuses the id, and this answer must not settle it.
    agent.responses.retain(|(k, _)| *k != SET_SELECT_ID);
    match acp::selected_value(&answer, &select) {
        // An answer that does not restate the select is taken at its word: ACP's response is the
        // agent saying the change was applied (`session/set_mode` answers `{}`).
        Ok(None) => Ok(()),
        Ok(Some(v)) if v == value => Ok(()),
        Ok(Some(v)) => refuse(agent, not_set(format!("the session is still on `{v}`"))),
        Err(e) => refuse(agent, not_set(e.to_string())),
    }
}

/// `session/prompt` — the first, and every one the node's inbox then holds — and the turn's expiry.
/// `false` when marion cut the session short: a prompt the agent never answered, or a hold that
/// outlived the wall clock.
///
/// Every prompt in flight settles before a boundary: the next queued message is then sent as a
/// prompt of its own, an empty inbox still owed a background child's end is waited on, and one
/// that seals ends the session. On a folding agent ([`marion_harness::spec::MidTurn::Fold`]) a
/// message queued mid-turn is sent at once, as S31 measured opencode and claude-agent-acp taking
/// it into the running loop and answering both prompts when it drains; on every other agent it
/// waits, because codex-acp never answers the first and copilot supersedes it.
///
/// The cancel is §8's interrupt step in ACP's own vocabulary, and it runs *before* the signal
/// because it is the cleaner one: the pending prompt answers a cancel with
/// `stopReason: "cancelled"`, which is a frame in the transcript, where a SIGKILL is a hole in it.
/// Written best-effort — an agent whose stdin has already closed is one the caller's kill handles.
fn prompt_session(
    agent: &mut Driver<'_>,
    session: &str,
    prompt: &str,
    deadline: Instant,
    turns: Option<&crate::inbox::TurnFeed>,
) -> Result<bool, AcpChildError> {
    let mut last_id = FIRST_PROMPT_ID;
    agent.send(
        &acp::prompt_request(last_id, session, prompt),
        "session/prompt",
    )?;
    let latch = Arc::new(crate::inbox::Latch::default());
    if let Some(feed) = turns {
        feed.source.attach_port(latch.clone());
    }
    let folding = turns.filter(|f| f.folds());
    let mut pending = vec![last_id];
    loop {
        while let Some(&id) = pending.first() {
            let mut sent = Vec::new();
            let answered = agent.settle_while(id, deadline, |agent| {
                let Some(feed) = folding.filter(|_| latch.take()) else {
                    return;
                };
                while let Some(msg) = feed.source.take_next() {
                    last_id += 1;
                    if send_turn(agent, feed, session, last_id, &msg, VIA_MID_TURN) {
                        sent.push(last_id);
                    }
                }
            });
            pending.remove(0);
            pending.extend(sent);
            if answered.is_none() {
                let _ = agent.write(&acp::cancel_notification(session));
                agent.settle(last_id, Instant::now() + CANCEL_GRACE);
                return Ok(false);
            }
        }
        let Some(feed) = turns else {
            return Ok(true);
        };
        match feed.source.take_or_seal() {
            Some(msg) => {
                last_id += 1;
                if !send_turn(agent, feed, session, last_id, &msg, VIA_NEXT_TURN) {
                    return Ok(true);
                }
                pending.push(last_id);
            }
            // Owed a background child's end: wait for the inbox, bounded by the wall clock.
            None if feed.source.held() => loop {
                if latch.take() {
                    break;
                }
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                if matches!(agent.child.try_wait(), Ok(Some(_))) {
                    return Ok(true);
                }
                // Answered while waiting: an agent may still ask marion something between turns.
                agent.classify();
                std::thread::sleep(POLL);
            },
            None => return Ok(true),
        }
    }
}

/// Send a taken message as `session/prompt` `id` and journal how it went. `false` when the agent's
/// stdin is gone — the message is dropped by name, and the session's end says why.
fn send_turn(
    agent: &mut Driver<'_>,
    feed: &crate::inbox::TurnFeed,
    session: &str,
    id: u64,
    msg: &crate::inbox::Message,
    via: &str,
) -> bool {
    let text = crate::inbox::render(msg);
    match agent.write(&acp::prompt_request(id, session, &text)) {
        Ok(()) => {
            feed.source.delivered(&msg.id, via);
            true
        }
        Err(e) => {
            feed.source
                .dropped(&msg.id, &format!("the agent's stdin closed: {e}"));
            false
        }
    }
}

/// What the adapter's session-opening request asks for, read back off the request itself.
struct Opening {
    /// The id the adapter stamped, for the driver to wait on.
    id: u64,
    /// `session/new` or `session/load` — the step name a refusal carries.
    method: &'static str,
    /// The session a `session/load` continues; `None` on a `session/new`, whose session is the
    /// agent's to name.
    session: Option<String>,
}

/// The request the adapter built, or a refusal naming what arrived instead. Two methods open a
/// session and both are accepted here; a load without a `sessionId` is refused before the spawn for
/// the same reason a request without an `id` is — it would be a wait on nothing.
fn declared(request: &Value) -> Result<Opening, AcpChildError> {
    let method = match request.get("method").and_then(Value::as_str) {
        Some(acp::SESSION_NEW_METHOD) => acp::SESSION_NEW_METHOD,
        Some(acp::SESSION_LOAD_METHOD) => acp::SESSION_LOAD_METHOD,
        Some(m) => {
            return Err(AcpChildError::Declaration {
                what: format!("its method is `{m}`"),
            });
        }
        None => {
            return Err(AcpChildError::Declaration {
                what: "it names no method at all".into(),
            });
        }
    };
    let id =
        request
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| AcpChildError::Declaration {
                what: "it carries no numeric `id`, so the agent's answer could not be told from a \
                   notification"
                    .into(),
            })?;
    let session = match method {
        acp::SESSION_LOAD_METHOD => Some(
            request
                .pointer("/params/sessionId")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| AcpChildError::Declaration {
                    what:
                        "it is a `session/load` naming no `sessionId`, so there is no session to \
                           continue"
                            .into(),
                })?,
        ),
        _ => None,
    };
    Ok(Opening {
        id,
        method,
        session,
    })
}

/// A step's deadline: its own measured budget, clipped to what is left of the caller's wall clock.
/// Never the later of the two — the caller's number is a ceiling, and a step budget that outran it
/// would make the wall clock advisory.
fn clip(overall: Instant, budget: Duration) -> Instant {
    let step = Instant::now()
        .checked_add(budget)
        .unwrap_or_else(|| Instant::now() + budget / 2);
    step.min(overall)
}

/// The last of `s`, bounded. The *last*, because a process that failed at startup says why in its
/// final lines and buries them under whatever it logged on the way there.
fn excerpt(s: &str) -> String {
    let s = s.trim_end();
    if s.len() <= STDERR_EXCERPT {
        return if s.is_empty() {
            "<empty>".into()
        } else {
            s.into()
        };
    }
    let cut = s.len() - STDERR_EXCERPT;
    let cut = (cut..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    format!("…{}", &s[cut..])
}

/// What the shutdown produced.
struct Finish {
    stdout: String,
    stderr: String,
    exit: ChildExit,
    capture_truncated: bool,
}

/// A spawned ACP agent with both pipes drained by threads, so a client-bound request can be
/// answered while frames are still arriving.
///
/// **Every exit from this struct kills the agent, and kills its group.** An ACP agent is a
/// long-lived stdio server that never closes stdout on its own — §8's leak check names exactly this
/// shape — and the agent's own children (marion's MCP bridge among them) inherit its pipes.
/// [`Drop`] is what makes that true on the paths that return an error; [`Driver::finish`] is the
/// one that also reports what the kill took.
struct Driver<'a> {
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    /// Every line the agent has written, in order. Shared with the reader thread.
    frames: Arc<Mutex<Vec<String>>>,
    /// The reader thread reached EOF, i.e. every holder of the stdout write end let go. False at
    /// the end of a run means the transcript is a prefix.
    stdout_eof: Arc<AtomicBool>,
    stderr: Option<Drain>,
    /// How far into `frames` the classifier has read. Requests are answered once and responses are
    /// indexed once, however many times a wait loop wakes.
    cursor: usize,
    /// `(id, frame)` for every response the agent has sent. Kept because S21 measured
    /// `session/update` notifications interleaved with, and arriving *before*, the response they
    /// belong to — so "the next line" is not the answer to anything, and an answer that arrives
    /// while marion is waiting on an earlier id must still be there when marion asks for it.
    responses: Vec<(u64, String)>,
    /// [`AcpChildSpec::on_line`]. Fed from [`Self::classify`], whose cursor already guarantees each
    /// line is read once, and topped up with the tail in [`Self::finish`].
    on_line: Option<&'a dyn Fn(&str)>,
}

impl<'a> Driver<'a> {
    fn spawn(inv: &Invocation, on_line: Option<&'a dyn Fn(&str)>) -> Result<Self, AcpChildError> {
        let mut cmd = inv.command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own group. The agent starts marion's MCP bridge from the `session/new` declaration
        // (S21 watched it happen), that bridge inherits both of the agent's pipes, and a `read` on
        // a pipe returns EOF only when *every* write end is closed. So killing the agent alone
        // leaves the drains wedged — `duplex` measured that deadlock — and killing the group is
        // what closes them. `kill_process_tree` refuses marion's own pgid, so without a group of
        // its own the sweep would have nothing it is allowed to address.
        cmd.process_group(0);
        let mut child = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
            .spawn(&mut cmd)
            .map_err(|source| AcpChildError::Spawn {
                program: inv.program.clone(),
                source,
            })?;
        let pid = child.id() as i32;
        let stdin = child.stdin.take();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stdout_eof = Arc::new(AtomicBool::new(false));
        let out = child.stdout.take().expect("stdout was piped");
        let sink = Arc::clone(&frames);
        let eof = Arc::clone(&stdout_eof);
        // Detached, and it has to be: the join is what would deadlock if anything the agent started
        // outlives it, so the group kill above is this thread's exit condition rather than a join.
        // Line-oriented because ACP's transport is newline-delimited and the driver answers
        // requests mid-turn — a whole-pipe read has nothing to answer with until the pipe closes.
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                sink.lock().expect("frame sink").push(line);
            }
            eof.store(true, Ordering::Relaxed);
        });
        let stderr = Drain::start(child.stderr.take().expect("stderr was piped"));
        Ok(Self {
            child,
            pid,
            stdin,
            frames,
            stdout_eof,
            stderr: Some(stderr),
            cursor: 0,
            responses: Vec::new(),
            on_line,
        })
    }

    /// One frame, newline-terminated. The transport is newline-delimited, so a frame that contained
    /// one would be two frames; `serde_json`'s compact form never does.
    fn write(&mut self, frame: &Value) -> std::io::Result<()> {
        let Some(w) = self.stdin.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "marion has already closed the agent's stdin",
            ));
        };
        writeln!(w, "{frame}")?;
        w.flush()
    }

    /// [`Self::write`], with the failure named after the step that could not be sent.
    fn send(&mut self, frame: &Value, step: &'static str) -> Result<(), AcpChildError> {
        match self.write(frame) {
            Ok(()) => Ok(()),
            Err(source) => {
                let _ = self.finish(true);
                Err(AcpChildError::Unwritable { step, source })
            }
        }
    }

    /// Wait for the response carrying `id`, **answering every client-bound request that arrives in
    /// the meantime**. `None` is "not by the deadline".
    ///
    /// The answering is not a courtesy. ACP is bidirectional: the agent sends marion requests
    /// mid-turn, and a request marion never answers stops the turn dead — the agent is waiting on
    /// marion, marion is waiting on the agent, and the only thing that ends it is the wall clock.
    /// S21's probe grew `answer_agent_requests` for this; `doctor` has no equivalent only because
    /// its micro-prompt is chosen so nothing is ever asked.
    fn settle(&mut self, id: u64, deadline: Instant) -> Option<String> {
        self.settle_while(id, deadline, |_| {})
    }

    /// [`Self::settle`], calling `between` on every poll — where a driver sends what it may send
    /// while the agent is still working (a folded prompt).
    fn settle_while(
        &mut self,
        id: u64,
        deadline: Instant,
        mut between: impl FnMut(&mut Self),
    ) -> Option<String> {
        let mut gone: Option<Instant> = None;
        loop {
            self.classify();
            between(self);
            if let Some((_, frame)) = self.responses.iter().find(|(k, _)| *k == id) {
                return Some(frame.clone());
            }
            // A dead agent will not answer, and waiting out a 60 s budget to discover that turns
            // "the agent crashed" into "the agent hung" — two findings §8 insists on telling apart.
            // The grace is because the process dying and its last frame arriving are not ordered:
            // the frame crosses a pipe and a mutex after the exit status is readable.
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                match gone {
                    Some(t) if t.elapsed() >= DEATH_GRACE => return None,
                    Some(_) => {}
                    None => gone = Some(Instant::now()),
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Read every frame the agent has written since the last call: index the responses, answer the
    /// requests, ignore the notifications.
    fn classify(&mut self) {
        let new: Vec<String> = {
            let seen = self.frames.lock().expect("frame sink");
            if seen.len() <= self.cursor {
                return;
            }
            let from = self.cursor;
            self.cursor = seen.len();
            seen[from..].to_vec()
        };
        for line in new {
            // Before the parse: a watcher sees what the agent wrote, banners included.
            if let Some(sink) = self.on_line {
                sink(&line);
            }
            // A line that is not JSON is kept in the transcript verbatim and classified as nothing.
            // Agents write banners and warnings to stdout, and `acp::json_frames` already skips
            // them; inventing a reply to one would be worse than ignoring it.
            let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let has_id = frame.get("id").is_some();
            if has_id && frame.get("method").is_some() {
                self.answer(&frame);
            } else if has_id
                && (frame.get("result").is_some() || frame.get("error").is_some())
                && let Some(k) = frame.get("id").and_then(Value::as_u64)
            {
                self.responses.push((k, line));
            }
        }
    }

    /// Answer one client-bound request.
    ///
    /// **Permissively, and the three kinds are not a policy marion invented here.** The shipped
    /// [`acp::initialize_request`] tells every agent that marion's client does `fs.readTextFile`,
    /// `fs.writeTextFile` and `terminal` before this driver sees a frame, so a refusal at this
    /// point would be a capability advertised and then withheld. And a denial would constrain
    /// nothing that is not already unconstrained: `AcpAdapter::compiled_permissions` records
    /// [`acp::NO_TOOL_AVAILABILITY_SURFACE`] because ACP has no field anywhere that narrows an
    /// agent's own tools, and S21's session had `write`, `edit` and `bash` in scope with marion
    /// asking for nothing. Denying the prompt while the agent holds `bash` would buy a slower turn
    /// and no safety.
    ///
    /// **The permission rule, stated once.** Every `session/request_permission` is answered with
    /// [`allow_option`]'s choice, whoever's tool it is:
    /// * *marion's own verbs* (`report`, `spawn`, …, in any agent's spelling — S22's claude shim
    ///   names `mcp__marion__report` in the ask, codex's only in the `tool_call` before it) are
    ///   approved because the bridge is the node's reason to exist, and an unapproved `report` is
    ///   a node that did the work and ends `Unreported`;
    /// * *the agent's other tools* follow the grant, and on ACP the grant is the agent's whole
    ///   toolset — the contract records [`acp::NO_TOOL_AVAILABILITY_SURFACE`] because the protocol
    ///   has no field that narrows it. An operator who wants the agent to stop asking at all states
    ///   the agent's own auto-accept mode as the type's `approval_mode`, which the driver sets as
    ///   the session mode before the prompt.
    ///
    /// Both are answered allow-once where the agent offers it, never allow-always (see
    /// [`allow_option`]), so no grant outlives the turn inside the agent's own settings.
    ///
    /// Anything else gets a JSON-RPC `-32601` naming the method — **answered, not dropped**, for
    /// `duplex`'s reason about unimplemented `control_request`s. `terminal/*` is the live case:
    /// marion advertises `terminal: true` and implements none of it, so a terminal-using agent
    /// learns that in one frame instead of hanging until the wall clock.
    fn answer(&mut self, request: &Value) {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").unwrap_or(&Value::Null).clone();
        let reply = match method {
            "session/request_permission" => match allow_option(&params) {
                Some(pick) => ok(
                    &id,
                    json!({"outcome": {"outcome": "selected", "optionId": pick}}),
                ),
                // Selecting an option that was not offered would be a fabricated answer; ACP's own
                // shape for "no option was taken" is an outcome, not an error, and it is the one
                // that lets the agent proceed.
                None => ok(&id, json!({"outcome": {"outcome": "cancelled"}})),
            },
            "fs/read_text_file" => match params.get("path").and_then(Value::as_str) {
                Some(p) => match std::fs::read_to_string(p) {
                    Ok(text) => ok(&id, json!({"content": text})),
                    Err(e) => err(&id, -32000, format!("{p}: {e}")),
                },
                None => err(&id, -32602, "fs/read_text_file names no `path`".into()),
            },
            // Performed, not acknowledged. S21's probe answered `{}` without writing, which is fine
            // for a probe and a lie to a node: an agent told its write succeeded goes on to read
            // the file back, or to report work it did not do.
            "fs/write_text_file" => match (
                params.get("path").and_then(Value::as_str),
                params.get("content").and_then(Value::as_str),
            ) {
                (Some(p), Some(c)) => match std::fs::write(p, c) {
                    Ok(()) => ok(&id, json!({})),
                    Err(e) => err(&id, -32000, format!("{p}: {e}")),
                },
                _ => err(
                    &id,
                    -32602,
                    "fs/write_text_file needs both `path` and `content`".into(),
                ),
            },
            other => err(
                &id,
                -32601,
                format!(
                    "marion's ACP client implements `session/request_permission`, \
                     `fs/read_text_file` and `fs/write_text_file`, and not `{other}`"
                ),
            ),
        };
        // Best-effort: an agent whose stdin has closed is being shut down anyway, and a write
        // failure here must not take down a turn whose frames are already worth reading.
        let _ = self.write(&reply);
    }

    /// Shut the agent down and collect everything.
    ///
    /// `marion_cut_it_short` is the caller's own statement that the turn did not finish, and it is
    /// what §6.7 calls an attributed kill — not something read off a signal number.
    fn finish(&mut self, marion_cut_it_short: bool) -> (Finish, usize) {
        // EOF on the agent's stdin first: it is the polite end of a stdio session, and an agent
        // that honours it exits before the signal.
        self.stdin.take();
        let interruptible = matches!(self.child.try_wait(), Ok(None));
        if interruptible {
            unsafe { kill(self.pid, SIGINT) };
        }
        let status = wait_bounded(&mut self.child, INTERRUPT_GRACE);
        let ended_on = if status.is_some() { SIGINT } else { SIGKILL };
        if status.is_none() {
            kill_process_tree(self.pid);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Unconditional, and it is what makes the drains below terminate. The agent is reaped by
        // here; what is not is whatever it started — marion's own MCP bridge, holding the write end
        // of both pipes. The group survives its leader, so `kill(-pgid)` still addresses them.
        kill_process_tree(self.pid);

        let drain_deadline = Instant::now() + DRAIN_GRACE;
        while !self.stdout_eof.load(Ordering::Relaxed) && Instant::now() < drain_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let stdout_complete = self.stdout_eof.load(Ordering::Relaxed);
        let collected = self.frames.lock().expect("frame sink").clone();
        // The lines no wait loop classified — whatever arrived after the last answer marion waited
        // for — so a watcher's view ends where the transcript does.
        if let Some(sink) = self.on_line {
            collected.iter().skip(self.cursor).for_each(|l| sink(l));
        }
        self.cursor = collected.len();
        let (stderr, stderr_complete) = match self.stderr.take() {
            Some(d) => d.finish(drain_deadline),
            None => (Vec::new(), true),
        };

        // **Marion's kill, reported as marion's kill.** On the short path the agent's own status is
        // not the turn's: an agent that raced the cancel and exited 0 would otherwise hand a
        // downstream reader an exit 0 for a turn marion cut off. `timed_out` is the field that says
        // what happened, and the code is dropped rather than reported as a success nobody earned.
        let exit = if marion_cut_it_short {
            ChildExit {
                code: None,
                signal: Some(ended_on),
                timed_out: true,
            }
        } else {
            ChildExit {
                code: status.as_ref().and_then(std::process::ExitStatus::code),
                signal: status
                    .as_ref()
                    .and_then(ExitStatusExt::signal)
                    .or((status.is_none()).then_some(SIGKILL)),
                timed_out: false,
            }
        };
        (
            Finish {
                stdout: collected.join("\n"),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                exit,
                capture_truncated: !(stdout_complete && stderr_complete),
            },
            collected.len(),
        )
    }

    /// Shut down and build a refusal out of what the agent left behind. The agent's stderr is the
    /// only thing an operator can act on when the frames say nothing, so no error path may skip it.
    fn refuse(&mut self, make: impl FnOnce(Finish, usize) -> AcpChildError) -> AcpChildError {
        let (end, frames) = self.finish(true);
        make(end, frames)
    }
}

impl Drop for Driver<'_> {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            unsafe { kill(self.pid, SIGKILL) };
            let _ = self.child.wait();
        }
        // The group, for the reason `finish` sweeps it: the agent's children hold its pipes, and a
        // driver dropped on an error path has the same obligation as one that returned a
        // transcript.
        kill_process_tree(self.pid);
    }
}

/// The option an agent's permission request should be answered with: `allow_once` where it is
/// offered, else any other allow, else the first option offered at all. Keyed on `kind` rather than
/// on `optionId`, because `kind` is ACP's own enumerated field (`allow_once`, `allow_always`,
/// `reject_once`, …) and the id is the agent's private string.
///
/// **Once, never "always", where the agent lets marion choose.** An `allow_always` is the agent
/// remembering the grant, and where it remembers it is the agent's business: claude-agent-acp
/// 0.66.0 attaches a `persistent`, `project_local` policy rule to it (S22's capture, for marion's
/// own `report`), which would write into the operator's project on marion's say-so (§6.4). A turn
/// that asks again is answered again; that costs a frame, not a hang.
pub fn allow_option(params: &Value) -> Option<String> {
    let options = params.get("options")?.as_array()?;
    let kind = |o: &&Value| {
        o.get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    options
        .iter()
        .find(|o| kind(o) == "allow_once")
        .or_else(|| options.iter().find(|o| kind(o).starts_with("allow")))
        .or_else(|| options.first())
        .and_then(|o| o.get("optionId")?.as_str())
        .map(str::to_string)
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: &Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// `Child::wait` with a ceiling. `None` is "still running at the deadline", which is a finding and
/// not an error.
fn wait_bounded(child: &mut Child, budget: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_testsupport::scratch;
    use std::path::Path;

    /// **A `session/load` must name a session**: an empty or blank `sessionId` would be a wait on
    /// a session no agent can open, so it is refused before the spawn; a real id is carried as
    /// written. Mutation: filter with `is_empty` alone and the blank id is accepted.
    #[test]
    fn a_session_load_naming_a_blank_session_is_refused() {
        let load = |id: &str| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 7, "method": acp::SESSION_LOAD_METHOD,
                "params": {"sessionId": id}
            })
        };
        for blank in ["", " \t "] {
            assert!(
                matches!(
                    declared(&load(blank)),
                    Err(AcpChildError::Declaration { .. })
                ),
                "{blank:?}"
            );
        }
        let opening = declared(&load("ses_prev")).expect("a named session loads");
        assert_eq!(opening.session.as_deref(), Some("ses_prev"));
        assert_eq!(opening.id, 7);
    }

    /// A fake ACP agent, in `/bin/sh` — `duplex`'s technique, for its reason: the thing under test
    /// is a driver of a *process over pipes*, and a fake that is a Rust function tests the loop
    /// with the pipes taken out of it.
    ///
    /// Every script here carries its own self-destruct, so a regression that would hang **fails**
    /// instead of wedging the suite.
    fn agent(cwd: &Path, script: &str) -> Invocation {
        Invocation {
            program: "sh".into(),
            args: vec!["-c".into(), format!("( sleep 20; kill -9 $$ ) &\n{script}")],
            env: vec![],
            env_remove: vec![],
            cwd: cwd.to_path_buf(),
            model: None,
            session_mode: None,
        }
    }

    /// The `initialize` result, in the shape `AgentHandshake::parse` accepts — wire v1 and a named
    /// agent, since anything else is refused before a session is opened.
    const HELLO: &str = r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentInfo":{"name":"fake-acp","version":"0.1"},"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"fork":{}}}}}"#;
    const OPENED: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_fake"}}"#;

    fn spec<'a>(
        inv: &'a Invocation,
        bound: Duration,
        on_started: &'a dyn Fn(i32),
    ) -> AcpChildSpec<'a> {
        AcpChildSpec {
            inv,
            session_declaration: None,
            prompt: "call marion's report tool",
            bound,
            on_started,
            on_line: None,
            turns: None,
        }
    }

    /// **A whole turn, end to end**, including the two things a probe never does: a client-bound
    /// request answered mid-turn, and a transcript that the shipped ACP reader parses into the call
    /// the model made.
    #[test]
    fn a_driven_turn_answers_the_agents_own_request_and_yields_a_transcript_marion_can_read() {
        let dir = scratch("acp-turn");
        let perm = dir.join("permission-answer.json");
        let prompt_seen = dir.join("prompt.json");
        // Handshake, session, then: ask marion for permission and **block on the answer**, which is
        // the hang this driver exists to prevent. Only after it arrives does the turn produce the
        // two frames that make up a marion tool call, and its `stopReason`.
        let script = format!(
            r#"read init
printf '%s\n' '{HELLO}'
read new
printf '%s\n' '{OPENED}'
read prompt
printf '%s\n' "$prompt" > '{prompt}'
printf '%s\n' '{{"jsonrpc":"2.0","id":91,"method":"session/request_permission","params":{{"sessionId":"ses_fake","options":[{{"optionId":"no","kind":"reject_once"}},{{"optionId":"yes-always","kind":"allow_always"}}]}}}}'
read answer
printf '%s\n' "$answer" > '{perm}'
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"ses_fake","update":{{"sessionUpdate":"tool_call","toolCallId":"c1","title":"marion_report","status":"pending","rawInput":{{}}}}}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"ses_fake","update":{{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","rawInput":{{"narrative":"hello from acp"}}}}}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
# A long-lived stdio server: it does not exit when the turn ends, and marion must kill it.
sleep 15"#,
            prompt = prompt_seen.display(),
            perm = perm.display(),
        );
        let inv = agent(&dir, &script);
        let run = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {}))
            .expect("the fake agent completes a turn");

        // The prompt marion sent is ACP's own shape, and reached the agent.
        let sent: Value =
            serde_json::from_str(&std::fs::read_to_string(&prompt_seen).unwrap()).unwrap();
        assert_eq!(sent["method"], "session/prompt");
        assert_eq!(sent["params"]["sessionId"], "ses_fake");
        assert_eq!(
            sent["params"]["prompt"][0]["text"],
            "call marion's report tool"
        );

        // The agent asked, and marion answered — with the option the agent labelled as an allow,
        // not with the first one in the list.
        let answered: Value = serde_json::from_str(&std::fs::read_to_string(&perm).unwrap())
            .expect("marion answered the permission request");
        assert_eq!(answered["id"], 91);
        assert_eq!(answered["result"]["outcome"]["optionId"], "yes-always");

        // **The transcript is the agent's own frames and only those.** marion's requests are not in
        // it, which is the shape `tests/fixtures/s21/opencode-acp-session.jsonl` has.
        assert!(
            !run.stdout.contains(r#""method":"session/prompt""#),
            "marion's own writes must not be in the agent's transcript: {}",
            run.stdout
        );
        assert_eq!(
            run.stdout.lines().count(),
            6,
            "the initialize and session/new results, the permission ask, two updates and the \
             prompt result — and nothing of marion's: {}",
            run.stdout
        );

        // And it is what the shipped reader reads: the verb, its outcome and its arguments.
        let spelling = acp::ToolSpelling::ServerUnderscoreTool;
        assert_eq!(
            acp::marion_calls(&run.stdout, spelling),
            vec![marion_harness::MarionCall {
                verb: "report".into(),
                outcome: marion_harness::CallOutcome::Answered,
            }]
        );
        let out = acp::parse_stream(&run.stdout, run.exit, spelling);
        assert_eq!(out.narrative.as_deref(), Some("hello from acp"));
        assert_eq!(out.failure, None);

        assert!(!run.exit.timed_out, "the turn answered inside its bound");
        // **Not truncated**, and that is a claim about the kill and not about the fake: the script
        // leaves a `sleep` holding the stdout write end after `sh` dies, so the capture only
        // completes because the whole process group is swept.
        assert!(
            !run.capture_truncated,
            "a pipe was still held open: the group sweep did not reach the agent's children"
        );
    }

    /// **A live sink sees each line while the turn is still running, and every line exactly once.**
    ///
    /// Causal rather than timed: the agent will not answer the prompt until the sink has seen its
    /// `agent_message_chunk` (the sink drops a marker file the script polls for), so a driver that
    /// only forwarded at the end would never be answered and the turn would expire. A non-JSON
    /// banner and a line written after the prompt's answer prove the sink is raw and complete.
    #[test]
    fn a_live_sink_sees_every_agent_line_in_order_before_the_turn_returns() {
        let dir = scratch("acp-live");
        let marker = dir.join("sink-saw-chunk");
        let script = format!(
            r#"read init
echo 'fake-acp banner, not JSON'
printf '%s\n' '{HELLO}'
read new
printf '%s\n' '{OPENED}'
read prompt
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"ses_fake","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"live"}}}}}}}}'
while [ ! -f '{marker}' ]; do sleep 0.05; done
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
echo 'after the answer'
sleep 15"#,
            marker = marker.display(),
        );
        let inv = agent(&dir, &script);
        let seen = Mutex::new(Vec::<String>::new());
        let on_line = |line: &str| {
            if line.contains("agent_message_chunk") {
                std::fs::write(&marker, "").unwrap();
            }
            seen.lock().unwrap().push(line.to_string());
        };
        let run = run_acp_child(AcpChildSpec {
            on_line: Some(&on_line),
            turns: None,
            ..spec(&inv, Duration::from_secs(10), &|_| {})
        })
        .expect("a turn");

        assert!(
            !run.exit.timed_out,
            "the prompt was never answered: the sink did not see the chunk while the turn ran"
        );
        let seen = seen.into_inner().unwrap();
        assert_eq!(
            seen,
            run.stdout.lines().map(str::to_string).collect::<Vec<_>>(),
            "the sink sees exactly the transcript's lines, in order, once each"
        );
        assert_eq!(
            seen.first().map(String::as_str),
            Some("fake-acp banner, not JSON")
        );
        assert!(seen.iter().any(|l| l == "after the answer"), "{seen:?}");
    }

    /// The `initialize` result of an agent that does **not** advertise `loadSession` — S20's
    /// `gemini --acp` shape, which carries no `agentCapabilities.loadSession` at all.
    const HELLO_NO_LOAD: &str = r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentInfo":{"name":"fake-acp","version":"0.1"},"agentCapabilities":{}}}"#;

    /// The declaration a resume compiles: `session/load` on the session the agent named last time,
    /// with the bridge declared again.
    fn load_declaration(cwd: &Path) -> Value {
        acp::session_load_request(
            marion_harness::adapter::SESSION_NEW_ID,
            "ses_prev",
            cwd,
            &[],
        )
    }

    /// **A resume is `session/load`, driven on the id the request carries.** The agent advertises
    /// `loadSession`, gets the load instead of a `session/new`, replays history (one update),
    /// answers with an empty result — ACP's own shape, no `sessionId` in it — and the prompt then
    /// goes out on the *requested* session. The replayed history is in the transcript, because it
    /// is the agent's own frames.
    #[test]
    fn a_resumed_turn_loads_the_named_session_and_prompts_on_it() {
        let dir = scratch("acp-load");
        let opened = dir.join("open.json");
        let prompt_seen = dir.join("prompt.json");
        let script = format!(
            r#"read init
printf '%s\n' '{HELLO}'
read open
printf '%s\n' "$open" > '{opened}'
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"ses_prev","update":{{"sessionUpdate":"user_message_chunk","content":{{"type":"text","text":"replayed"}}}}}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":null}}'
read prompt || exit 0
printf '%s\n' "$prompt" > '{prompt}'
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
sleep 15"#,
            opened = opened.display(),
            prompt = prompt_seen.display(),
        );
        let inv = agent(&dir, &script);
        let run = run_acp_child(AcpChildSpec {
            session_declaration: Some(load_declaration(&dir)),
            ..spec(&inv, Duration::from_secs(20), &|_| {})
        })
        .expect("the fake agent loads the session and completes a turn");

        let sent: Value = serde_json::from_str(&std::fs::read_to_string(&opened).unwrap()).unwrap();
        assert_eq!(sent["method"], acp::SESSION_LOAD_METHOD);
        assert_eq!(sent["params"]["sessionId"], "ses_prev");
        let prompted: Value =
            serde_json::from_str(&std::fs::read_to_string(&prompt_seen).unwrap()).unwrap();
        assert_eq!(prompted["method"], "session/prompt");
        assert_eq!(
            prompted["params"]["sessionId"], "ses_prev",
            "the prompt continues the session the load named, not one the answer invented"
        );
        assert!(
            run.stdout.contains("replayed"),
            "the replayed history is the agent's own frames and stays in the transcript"
        );
        assert!(!run.exit.timed_out);
    }

    /// **An agent without `loadSession` is refused a resume by name, before the load is sent.** The
    /// protocol has no other resume marion drives, so the refusal names the capability the agent
    /// did not advertise and the agent it was — and the agent sees no `session/load` at all, which
    /// the script proves by exiting on the frame it does receive.
    #[test]
    fn a_resume_is_refused_by_name_when_the_agent_does_not_advertise_load_session() {
        let dir = scratch("acp-noload");
        let seen = dir.join("after-hello.json");
        let script = format!(
            r#"read init
printf '%s\n' '{HELLO_NO_LOAD}'
read next && printf '%s\n' "$next" > '{seen}'
sleep 15"#,
            seen = seen.display(),
        );
        let inv = agent(&dir, &script);
        let e = run_acp_child(AcpChildSpec {
            session_declaration: Some(load_declaration(&dir)),
            ..spec(&inv, Duration::from_secs(20), &|_| {})
        })
        .expect_err("no `loadSession`, no load");
        assert!(
            matches!(&e, AcpChildError::NoLoadSession { agent, .. } if agent.contains("fake-acp")),
            "the refusal names the capability and the agent: {e}"
        );
        assert!(
            e.to_string().contains("loadSession") && e.to_string().contains("ses_prev"),
            "{e}"
        );
        assert!(
            !seen.exists(),
            "the agent must never have been sent a frame after `initialize`: {:?}",
            std::fs::read_to_string(&seen)
        );
    }

    /// **The wall clock, and a timeout that is not a lie.** The agent takes the prompt and never
    /// answers it. What must survive is everything it *did* say, plus its own `cancelled` — and the
    /// exit must say marion ended this, never exit 0.
    #[test]
    fn an_expired_turn_keeps_its_frames_and_reports_marions_own_kill() {
        let dir = scratch("acp-expiry");
        let script = format!(
            r#"read init
printf '%s\n' '{HELLO}'
read new
printf '%s\n' '{OPENED}'
read prompt
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"ses_fake","update":{{"sessionUpdate":"agent_thought_chunk","content":{{"type":"text","text":"thinking"}}}}}}}}'
# The turn never answers. marion's `session/cancel` is what this reads, and ACP's own answer to a
# cancel is the pending prompt returning `cancelled`.
read cancel
printf '%s\n' "$cancel" > '{seen}'
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"cancelled"}}}}'
sleep 15"#,
            seen = dir.join("cancel.json").display(),
        );
        let inv = agent(&dir, &script);
        let started = Instant::now();
        let run = run_acp_child(spec(&inv, Duration::from_secs(2), &|_| {}))
            .expect("an expired turn is a transcript, not an error");

        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the bound was honoured"
        );
        // The cancel marion sent is ACP's notification shape: no id, or the agent would owe it an
        // answer instead of ending the prompt.
        let cancel: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("cancel.json")).unwrap())
                .expect("marion sent session/cancel on expiry");
        assert_eq!(cancel["method"], "session/cancel");
        assert!(cancel.get("id").is_none());

        // Everything the agent said before the expiry is still here, and so is its own last word.
        assert!(run.stdout.contains("agent_thought_chunk"), "{}", run.stdout);
        assert!(
            run.stdout.contains(r#""stopReason":"cancelled""#),
            "{}",
            run.stdout
        );

        // **Never an exit 0.** `timed_out` is marion's attributed kill (§6.7) and the code is
        // dropped rather than reported as a success the turn did not reach.
        assert!(run.exit.timed_out);
        assert_eq!(run.exit.code, None);
        assert!(run.exit.signal.is_some());
    }

    /// **An agent that never answers `initialize` is refused by name**, with its own stderr
    /// attached — which is the only thing an operator can act on when there are no frames. S22's
    /// zero-frame `npx` probe is this case, and it was a cold download rather than a broken agent.
    #[test]
    fn an_agent_that_never_answers_initialize_is_refused_by_name() {
        let dir = scratch("acp-mute");
        // Reads marion's `initialize`, says nothing on stdout, complains on stderr and dies.
        let inv = agent(
            &dir,
            "read init\necho 'fake-acp: this build has no ACP support' >&2\nexit 3",
        );
        let started = Instant::now();
        let e = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {}))
            .expect_err("no handshake, no run");
        match &e {
            AcpChildError::NoHandshake { frames, stderr, .. } => {
                assert_eq!(*frames, 0);
                assert!(stderr.contains("no ACP support"), "{stderr}");
            }
            other => panic!("expected NoHandshake, got {other:?}"),
        }
        assert!(e.to_string().contains("initialize"), "{e}");
        // And a dead agent is not waited out: the handshake budget is 30 s and this must not spend
        // it, or "the agent crashed" and "the agent hung" become the same finding.
        assert!(
            started.elapsed() < HANDSHAKE_BUDGET / 2,
            "waited {:?} on an agent that had already exited",
            started.elapsed()
        );
    }

    /// **§6.1 step 7's ordering: the pid is confirmed before one byte reaches the node.** The
    /// callback here does what the real one does — a durable write — and the agent records whether
    /// that write had landed by the time marion's first frame arrived.
    #[test]
    fn the_pid_is_confirmed_before_the_first_byte_is_written_to_the_agent() {
        let dir = scratch("acp-order");
        let marker = dir.join("started");
        let order = dir.join("order");
        let script = format!(
            r#"read init
if [ -f '{marker}' ]; then printf 'confirmed-first\n' > '{order}'; else printf 'wrote-first\n' > '{order}'; fi
printf '%s\n' '{HELLO}'
read new
printf '%s\n' '{OPENED}'
read prompt
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
sleep 15"#,
            marker = marker.display(),
            order = order.display(),
        );
        let inv = agent(&dir, &script);
        let seen = Mutex::new(Vec::new());
        let on_started = |pid: i32| {
            seen.lock().unwrap().push(pid);
            // A journal append and an fsync take real time, and the ordering must hold because the
            // call happens first — not because it happens to be fast.
            std::thread::sleep(Duration::from_millis(300));
            std::fs::write(&marker, pid.to_string()).unwrap();
        };
        let run = run_acp_child(spec(&inv, Duration::from_secs(20), &on_started)).expect("a turn");

        assert_eq!(
            std::fs::read_to_string(&order).unwrap().trim(),
            "confirmed-first",
            "the agent had already been handed marion's `initialize` when the pid was confirmed"
        );
        let pids = seen.into_inner().unwrap();
        assert_eq!(pids.len(), 1, "confirmed once, not once per frame");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            pids[0].to_string(),
            "the pid handed to the caller is the one that was spawned"
        );
        assert!(pids[0] > 1);
        assert!(run.stdout.contains("end_turn"));
    }

    /// A declaration that is not a `session/new` request is refused **before anything is spawned**.
    /// The id is read out of the adapter's own frame, so a driver and an adapter that disagreed
    /// about it would hang rather than fail — this is what keeps that from being possible.
    #[test]
    fn a_declaration_marion_cannot_correlate_against_is_refused_before_a_process_exists() {
        let dir = scratch("acp-decl");
        // A program that does not exist: reaching the spawn at all is the failure being excluded.
        let inv = Invocation {
            program: "/nonexistent/acp-agent".into(),
            args: vec![],
            env: vec![],
            env_remove: vec![],
            cwd: dir.to_path_buf(),
            model: None,
            session_mode: None,
        };
        let refuse = |d: Value| {
            run_acp_child(AcpChildSpec {
                inv: &inv,
                session_declaration: Some(d),
                prompt: "hi",
                bound: Duration::from_secs(5),
                on_started: &|_| panic!("nothing may be spawned"),
                on_line: None,
                turns: None,
            })
            .expect_err("refused")
        };
        assert!(matches!(
            refuse(json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt"})),
            AcpChildError::Declaration { .. }
        ));
        let anonymous = refuse(json!({"jsonrpc": "2.0", "method": "session/new", "params": {}}));
        assert!(
            matches!(&anonymous, AcpChildError::Declaration { what } if what.contains("`id`")),
            "{anonymous}"
        );
        // The real one passes this gate and fails at the spawn, which is the next thing that can go
        // wrong and proves the gate is not refusing everything.
        let real = acp::session_new_request(marion_harness::adapter::SESSION_NEW_ID, &dir, &[]);
        assert!(matches!(refuse(real), AcpChildError::Spawn { .. }));
    }

    /// The permission answer is chosen by ACP's `kind`, not by position: an agent that lists its
    /// rejection first must not be answered with it.
    #[test]
    fn the_permission_option_is_chosen_by_its_kind_and_a_request_with_none_is_still_answered() {
        let offered = json!({"options": [
            {"optionId": "n", "kind": "reject_once"},
            {"optionId": "y", "kind": "allow_once"},
        ]});
        assert_eq!(allow_option(&offered).as_deref(), Some("y"));
        // No allow on offer: the agent's own first option, rather than an id marion invented.
        assert_eq!(
            allow_option(&json!({"options": [{"optionId": "only", "kind": "reject_always"}]}))
                .as_deref(),
            Some("only")
        );
        assert_eq!(allow_option(&json!({"options": []})), None);
        assert_eq!(allow_option(&Value::Null), None);
    }

    /// `tests/fixtures/acp/fake_acp_agent.py`, as an [`Invocation`] asking for `model`. `None`
    /// where `python3` is absent, so the caller skips by name rather than failing on the machine.
    fn fake_agent(cwd: &Path, model: Option<&str>) -> Option<Invocation> {
        if !marion_testsupport::on_path("python3") {
            eprintln!("skipped: `python3` is not installed");
            return None;
        }
        Some(Invocation {
            program: "python3".into(),
            args: vec![
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fixtures/acp/fake_acp_agent.py"
                )
                .into(),
            ],
            env: vec![],
            env_remove: vec![],
            cwd: cwd.to_path_buf(),
            model: model.map(str::to_string),
            session_mode: None,
        })
    }

    /// The fake agent's model option's `currentValue`, in every frame of the transcript that
    /// restates it, in order.
    fn models_seen(stdout: &str) -> Vec<String> {
        stdout
            .lines()
            .filter_map(|l| acp::session_select(l, acp::MODEL_CATEGORY)?.current)
            .collect()
    }

    /// **The requested model reaches the session over ACP itself** — the live matrix's D4, where an
    /// `opencode acp` child ran on whatever its own default was and the contract recorded no model.
    /// The agent advertised a `model` select in `session/new`, marion set it with
    /// `session/set_config_option`, and only then prompted.
    #[test]
    fn a_requested_model_is_set_through_the_session_before_the_prompt() {
        let dir = scratch("acp-model-set");
        let Some(inv) = fake_agent(&dir, Some("fake/beta")) else {
            return;
        };
        let run = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {}))
            .expect("the agent offers the model, so the turn runs");
        assert_eq!(
            models_seen(&run.stdout),
            vec!["fake/alpha", "fake/beta"],
            "the session opened on the agent's default and was then set to the request: {}",
            run.stdout
        );
        assert!(
            dir.join("src/marion_acp.txt").exists(),
            "the prompt ran after the model was set"
        );
    }

    /// A request the session already satisfies sends nothing: the transcript restates the model
    /// once, in the `session/new` answer.
    #[test]
    fn a_model_the_session_already_runs_is_not_set_again() {
        let dir = scratch("acp-model-same");
        let Some(inv) = fake_agent(&dir, Some("fake/alpha")) else {
            return;
        };
        let run = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {})).unwrap();
        assert_eq!(
            models_seen(&run.stdout),
            vec!["fake/alpha"],
            "{}",
            run.stdout
        );
    }

    /// **A model the agent does not offer is refused by name, before the prompt** — rather than a
    /// turn on the agent's own default under a contract naming the request.
    #[test]
    fn a_model_the_agent_does_not_offer_is_refused_by_name_before_any_prompt() {
        let dir = scratch("acp-model-absent");
        let Some(inv) = fake_agent(&dir, Some("fake/gamma")) else {
            return;
        };
        let e = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {}))
            .expect_err("fake/gamma is not on offer");
        let text = e.to_string();
        assert!(
            matches!(&e, AcpChildError::NotOffered { value, category: "model", .. } if value == "fake/gamma"),
            "{text}"
        );
        assert!(
            text.contains("fake/alpha") && text.contains("fake/beta"),
            "names what is offered: {text}"
        );
        assert!(
            !dir.join("src/marion_acp.txt").exists(),
            "no prompt was sent"
        );
    }

    /// An agent that advertises no model select cannot be given one, and saying so beats a run on
    /// a model nobody chose (copilot's ACP session, S28, offers none).
    #[test]
    fn a_model_asked_of_an_agent_with_no_model_select_is_refused_by_name() {
        let dir = scratch("acp-model-none");
        let prompt_seen = dir.join("prompt.json");
        let script = format!(
            r#"read init
printf '%s\n' '{HELLO}'
read new
printf '%s\n' '{OPENED}'
read prompt
printf '%s\n' "$prompt" > '{prompt}'
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
sleep 15"#,
            prompt = prompt_seen.display(),
        );
        let inv = Invocation {
            model: Some("fake/beta".into()),
            ..agent(&dir, &script)
        };
        let e = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {}))
            .expect_err("nothing to set the model with");
        assert!(
            matches!(&e, AcpChildError::NoSelect { value, category: "model", .. } if value == "fake/beta"),
            "{e}"
        );
        // What the agent read after the session, if anything: a `read` torn by marion's shutdown
        // may still run the next line, so the claim is on the content, not the file's existence.
        let after = std::fs::read_to_string(&prompt_seen).unwrap_or_default();
        assert!(
            !after.contains("session/prompt"),
            "no prompt was sent: {after}"
        );
    }

    /// **An agent type's `approval_mode` is set as the session's mode, over the protocol, after
    /// the model** — the generic route to an agent's own auto-accept (claude-agent-acp's
    /// `acceptEdits`, copilot's `#autopilot`), keyed on ACP's `mode` category.
    #[test]
    fn a_requested_approval_mode_is_set_as_the_sessions_mode_before_the_prompt() {
        let dir = scratch("acp-mode-set");
        let Some(inv) = fake_agent(&dir, Some("fake/beta")) else {
            return;
        };
        let inv = Invocation {
            session_mode: Some("auto".into()),
            ..inv
        };
        let run = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {})).unwrap();
        let modes: Vec<String> = run
            .stdout
            .lines()
            .filter_map(|l| acp::session_select(l, acp::MODE_CATEGORY)?.current)
            .collect();
        assert_eq!(
            modes,
            vec!["ask", "ask", "auto"],
            "opened on `ask`, restated by the model's answer, then set: {}",
            run.stdout
        );
        assert_eq!(
            models_seen(&run.stdout).last().map(String::as_str),
            Some("fake/beta"),
            "the model is still the one asked for"
        );
        assert!(dir.join("src/marion_acp.txt").exists());
    }

    /// A mode the agent does not offer is refused by name, naming what it does offer.
    #[test]
    fn an_approval_mode_the_agent_does_not_offer_is_refused_by_name() {
        let dir = scratch("acp-mode-absent");
        let Some(inv) = fake_agent(&dir, None) else {
            return;
        };
        let inv = Invocation {
            session_mode: Some("bypassPermissions".into()),
            ..inv
        };
        let e = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {})).unwrap_err();
        assert!(
            matches!(&e, AcpChildError::NotOffered { category: "mode", value, .. } if value == "bypassPermissions"),
            "{e}"
        );
        assert!(e.to_string().contains("ask, auto"), "{e}");
        assert!(
            !dir.join("src/marion_acp.txt").exists(),
            "no prompt was sent"
        );
    }

    /// **Every permission ask is answered allow-once where the agent offers it, never "always".**
    /// claude-agent-acp's `allow_always` for marion's own `report` carries a persistent,
    /// `project_local` policy rule (S22's capture), so choosing it would write into the operator's
    /// project; codex-acp offers two `allow_always` kinds beside `allow_once`. Both captures are
    /// asks for marion's own `report`, and the same rule answers any other tool (see
    /// `Driver::answer`).
    #[test]
    fn a_permission_ask_is_answered_allow_once_on_both_measured_shapes() {
        let claude = json!({"options": [
            {"optionId": "reject", "kind": "reject_once"},
            {"optionId": "allow_always", "kind": "allow_always"},
            {"optionId": "allow", "kind": "allow_once"},
        ]});
        assert_eq!(allow_option(&claude).as_deref(), Some("allow"));
        let codex = json!({"options": [
            {"optionId": "allow_session", "kind": "allow_always"},
            {"optionId": "allow_once", "kind": "allow_once"},
            {"optionId": "decline", "kind": "reject_once"},
        ]});
        assert_eq!(allow_option(&codex).as_deref(), Some("allow_once"));
        // Only "always" on offer: taken, because an unanswered ask hangs the turn.
        assert_eq!(
            allow_option(&json!({"options": [
                {"optionId": "r", "kind": "reject_once"},
                {"optionId": "a", "kind": "allow_always"},
            ]}))
            .as_deref(),
            Some("a")
        );
    }

    // ---- turn delivery (S31): the node's inbox, as prompts after the first ----

    use crate::inbox::{BoundInbox, Inboxes, Message, Source, TurnFeed, render};
    use marion_core::contract::{AgentId, TaskId};
    use marion_core::journal::RecordKind;
    use marion_harness::spec::{MidTurn, TurnDelivery};

    struct Fed {
        inboxes: Arc<Inboxes>,
        log: Arc<Mutex<Vec<RecordKind>>>,
        agent: AgentId,
        feed: TurnFeed,
    }

    fn fed(mid_turn: MidTurn) -> Fed {
        let (inboxes, log) = crate::inbox::tests::recording();
        let inboxes = Arc::new(inboxes);
        let agent = AgentId("acp-node".into());
        inboxes.open(&agent);
        let feed = TurnFeed::new(
            Arc::new(BoundInbox::new(Arc::clone(&inboxes), agent.clone())),
            TurnDelivery::TypedTurn {
                mid_turn,
                note: "t",
            },
        );
        Fed {
            inboxes,
            log,
            agent,
            feed,
        }
    }

    impl Fed {
        fn delivery(&self) -> TurnDelivery {
            TurnDelivery::TypedTurn {
                mid_turn: self.feed.mid_turn,
                note: "t",
            }
        }
        fn queue(&self, source: Source, text: &str) -> (String, String) {
            let id = self
                .inboxes
                .enqueue(&self.agent, self.delivery(), source.clone(), text.into())
                .expect("open");
            (id, rendered(source, text))
        }
        fn delivered(&self) -> Vec<(String, String)> {
            crate::inbox::tests::records(&self.log)
                .into_iter()
                .filter_map(|r| match r {
                    RecordKind::MessageDelivered(d) => Some((d.message_id, d.via)),
                    _ => None,
                })
                .collect()
        }
        fn sealed(&self) -> bool {
            self.inboxes
                .enqueue(
                    &self.agent,
                    self.delivery(),
                    Source::Operator,
                    "late".into(),
                )
                .is_err()
        }
        fn spec<'a>(&self, inv: &'a Invocation) -> AcpChildSpec<'a> {
            AcpChildSpec {
                turns: Some(self.feed.clone()),
                ..spec(inv, Duration::from_secs(20), &|_| {})
            }
        }
    }

    fn rendered(source: Source, text: &str) -> String {
        render(&Message {
            id: String::new(),
            source,
            text: text.into(),
            queued_at: std::time::SystemTime::now(),
        })
    }

    /// `(id, text)` of the `session/prompt` a script wrote to `path`.
    fn prompt_at(path: &Path) -> (u64, String) {
        let line = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("the agent never read a prompt into {path:?}: {e}"));
        let v: Value = serde_json::from_str(line.trim()).expect("a frame");
        assert_eq!(v["method"], "session/prompt", "{v}");
        assert_eq!(v["params"]["sessionId"], "ses_fake");
        (
            v["id"].as_u64().expect("an id"),
            v["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
    }

    fn wait_for_file(path: &Path) {
        let until = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(Instant::now() < until, "{path:?} never appeared");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    const HANDSHAKE: &str =
        "read init\nprintf '%s\\n' \"$HELLO\"\nread new\nprintf '%s\\n' \"$OPENED\"\n";

    fn fed_agent(dir: &Path, body: &str) -> Invocation {
        agent(
            dir,
            &format!("HELLO='{HELLO}'\nOPENED='{OPENED}'\n{HANDSHAKE}{body}"),
        )
    }

    /// **On a queueing agent a message waits for the running prompt to settle**, then goes out as
    /// the next prompt under the next id — never while the first is in flight, which codex-acp
    /// would never answer and copilot would supersede (S31).
    #[test]
    fn a_queueing_agent_gets_the_message_as_its_next_prompt_only_after_the_first_settles() {
        let dir = scratch("acp-queue");
        let (held, early, second) = (dir.join("held"), dir.join("early"), dir.join("second"));
        let fx = fed(MidTurn::Queue);
        let script = format!(
            r#"read -r prompt
: > '{held}'
if read -r -t 2 line; then printf '%s\n' "$line" > '{early}'; fi
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
read -r next
printf '%s\n' "$next" > '{second}'
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'
read -r more
exit 0"#,
            held = held.display(),
            early = early.display(),
            second = second.display(),
        );
        let inv = fed_agent(&dir, &script);
        let (id, want) = std::thread::scope(|s| {
            let steer = s.spawn(|| {
                wait_for_file(&held);
                fx.queue(Source::Operator, "and the tests")
            });
            let run = run_acp_child(fx.spec(&inv)).expect("the session completes");
            assert!(!run.exit.timed_out, "marion did not cut it short");
            steer.join().unwrap()
        });
        assert!(
            !early.exists(),
            "a prompt was sent while the first was in flight"
        );
        assert_eq!(prompt_at(&second), (3, want));
        assert_eq!(fx.delivered(), [(id, VIA_NEXT_TURN.to_string())]);
        assert!(fx.sealed());
    }

    /// **On a folding agent a message is sent into the running turn** under its own id, and the
    /// session ends only once both prompts are answered — which S31 measured arriving together
    /// when the loop drains.
    #[test]
    fn a_folding_agent_gets_the_message_mid_turn_and_both_prompts_settle() {
        let dir = scratch("acp-fold");
        let (held, mid) = (dir.join("held"), dir.join("mid"));
        let fx = fed(MidTurn::Fold);
        let script = format!(
            r#"read -r prompt
: > '{held}'
read -r line
printf '%s\n' "$line" > '{mid}'
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
sleep 0.3
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn","usage":{{"inputTokens":1,"outputTokens":1}}}}}}'
read -r more
exit 0"#,
            held = held.display(),
            mid = mid.display(),
        );
        let inv = fed_agent(&dir, &script);
        let (id, want) = std::thread::scope(|s| {
            let steer = s.spawn(|| {
                wait_for_file(&held);
                fx.queue(Source::Operator, "switch approach")
            });
            let run = run_acp_child(fx.spec(&inv)).expect("the session completes");
            assert!(!run.exit.timed_out);
            assert!(
                run.stdout.contains(r#""id":3"#),
                "the driver waited for the folded prompt's answer too: {}",
                run.stdout
            );
            steer.join().unwrap()
        });
        assert_eq!(prompt_at(&mid), (3, want));
        assert_eq!(fx.delivered(), [(id, VIA_MID_TURN.to_string())]);
        assert!(fx.sealed());
    }

    /// **An agent owed a background child's end is held open after its prompt settles**, and
    /// the end, announced later, is its next prompt.
    #[test]
    fn an_agent_owed_a_childs_end_is_held_and_takes_it_as_its_next_prompt() {
        let dir = scratch("acp-held");
        let (done, second) = (dir.join("done"), dir.join("second"));
        let fx = fed(MidTurn::Fold);
        assert!(fx.inboxes.owe(&fx.agent));
        let ended = Source::ChildEnded {
            child: AgentId("child".into()),
            task_id: TaskId("t-child".into()),
            status: "completed".into(),
            agent_type: "claude".into(),
            root: false,
        };
        let script = format!(
            r#"read -r prompt
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
: > '{done}'
read -r next
printf '%s\n' "$next" > '{second}'
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'
read -r more
exit 0"#,
            done = done.display(),
            second = second.display(),
        );
        let inv = fed_agent(&dir, &script);
        let id = std::thread::scope(|s| {
            let child = s.spawn(|| {
                wait_for_file(&done);
                std::thread::sleep(Duration::from_millis(300));
                fx.inboxes
                    .announce(
                        &fx.agent,
                        fx.delivery(),
                        ended.clone(),
                        "the contract".into(),
                    )
                    .expect("owed, so accepted")
            });
            let run = run_acp_child(fx.spec(&inv)).expect("the session completes");
            assert!(!run.exit.timed_out);
            child.join().unwrap()
        });
        assert_eq!(prompt_at(&second), (3, rendered(ended, "the contract")));
        assert_eq!(fx.delivered(), [(id, VIA_NEXT_TURN.to_string())]);
        assert!(fx.sealed());
    }
}
