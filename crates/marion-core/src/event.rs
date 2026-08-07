//! `events.jsonl` — **one node's stream**, its records and their framing (design §4, §7.3.3).
//!
//! ```text
//! <state>/<project-hash>/agents/<agent_id>/events.jsonl      # IR, append-only
//! ```
//!
//! `ir.rs` says the full `Event` of §4 *"lands with `events.jsonl`"*. This is that landing. As with
//! the journal, this module is pure data plus the line codec and the fold; the file lives in the
//! supervisor (`marion_supervisor::events`), because `marion-core` performs no I/O.
//!
//! # What is in this file is defined by what a live attach delivers, not the other way round
//!
//! This is the load-bearing sentence of the whole design, and it is forced rather than chosen.
//! §7.3.3 makes re-attach **both** legs at once — a node that finished while detached is *replayed*
//! from this file, a node still running is *re-subscribed* to, and a node that was running at
//! detach and finished before re-attach needs both — so the two legs are spliced together in one
//! client, at one seam. **If replay and subscribe do not share a vocabulary, the seam is not a seam
//! but a translation**, and every hazard §7.3.3 raises comes back as a mapping bug at the join.
//! So the contents of this file are *derived* from what a subscriber receives, and the rule for
//! adding anything here is the same question every time: would a live attach deliver it?
//!
//! # An event is not a journal record
//!
//! | | `journal.jsonl` | `events.jsonl` |
//! |---|---|---|
//! | scope | the project — every node | one node |
//! | answers | *what nodes exist, and what did marion decide about them* | *what did this node's channel carry* |
//! | provenance | `Source::Marion` on every record, always | normally the harness — `Protocol`, `Transcript`, `Pty` |
//! | who reads it | the registry, to build a tree | a client, to render a stream |
//!
//! What is deliberately **not** here, each because it already has a home (§4.3's discipline is one
//! fact, one home):
//!
//! * **the tree** — parenthood, depth, `agent_type`, existence. All journal, and nothing here is
//!   ever folded into a tree. `parent_id` is on §4's `Event` and is omitted for exactly this
//!   reason: §7.5 makes it immutable and the journal is its source, so a copy here would be a
//!   second answer to a question that must only have one.
//! * **the result** — `contracts/<task_id>.json` is §6.7's audit record of what a run *produced*.
//!   This is the stream that produced it.
//! * **the terminal grid** — `pty.cast`, with its own relative timebase. [`Event::mono_ns`] exists
//!   precisely to align the two (§4.2) rather than to merge them.
//!
//! **The one deliberate overlap is the lifecycle bookends**, and it is argued rather than hidden.
//! A stream of frames with no terminal event cannot distinguish *"the node finished"* from *"the
//! stream stopped mid-turn"* — which is `marion_harness::CallOutcome::Unknown`'s reasoning exactly,
//! and §7.3.3 requires a client that was absent for a node's **entire** life to learn that node
//! from this file. So [`Lifecycle`] bookends the stream. That the journal also records the same
//! instant is not two sources of truth: §4.1's `authoritative` axis exists *because* the same fact
//! legitimately arrives twice from two vantage points, and nothing reads this file to decide what
//! nodes exist.
//!
//! # §4's other eight payloads are refused **in the type**, not in a comment
//!
//! §4 lists eleven `Payload` variants. marion can source three today, and the other five that carry
//! meaning — `Message`, `ToolCall`, `ToolResult`, `Permission`, `Usage` — require a per-frame
//! normalization that **does not exist in this workspace**: the adapters' `parse_stream` produces a
//! `StreamOutcome`, an aggregate over a whole run, and no code anywhere turns a codex
//! `item.completed` into a `ToolResult`. Emitting them now would mean inventing a mapping and
//! stamping `Transformation::Normalized` on a guess, which is what `handler.rs`'s `Unprojectable`
//! exists to refuse.
//!
//! Refusing in prose would let a future implementer read past it. So the refusal is
//! [`Payload::Normalized`], carrying [`Normalization`] — an **uninhabited** enum. The variant is
//! there, named and documented; no code can construct one and no line can decode to one, and the
//! only way to emit a normalized payload is to add a variant to `Normalization`, which is the
//! moment the mapping has to be written down. The absence is something you hit, not something you
//! skim.
//!
//! # Framing, and why the journal's 16 KiB cap does **not** transfer
//!
//! The framing is the journal's, unchanged: one record per line, compact JSON, `\n`-terminated, so
//! the delimiter *is* the frame and a torn tail is a prefix of exactly one line.
//!
//! The **cap** is a different matter, and the shallow reason is not the real one. The shallow
//! reason is that the journal's cap buys `O_APPEND` atomicity between two writer processes and this
//! file has one writer (see `marion_supervisor::events`), so there is no interleaving to defend
//! against. The real reason is about vocabulary:
//!
//! > **A journal record cannot be shortened, because a shortened one parses as a different, wrong
//! > record and the journal's vocabulary has no word for "shortened". An event can be, because
//! > §4.1's `Completeness::Partial` is exactly that word.**
//!
//! So over-size here is not a refusal. A `Vendor` frame carrying a large tool result, or a `Raw`
//! line from a harness that dumped a file, legitimately exceeds any bound worth setting — and
//! refusing to record a node's real output would be **data loss dressed as safety**. [`bound`]
//! replaces such a payload with [`Payload::Oversized`] at `Completeness::Partial`: the event still
//! exists, at its right ordinal, saying honestly that its body was too large. The ordinal is the
//! part that matters, because §7.3.3's seam is stated in ordinals.
//!
//! [`MAX_EVENT_BYTES`] is therefore not an atomicity bound at all. It bounds how large a single
//! line a reader must buffer, and it is now **measured against this repository's captures** rather
//! than guessed — see the constant.
//!
//! # Growth: nothing, and compaction is unavailable in principle rather than merely unbuilt
//!
//! A per-node event stream from a long-running interactive node is unbounded, and there is no
//! compaction here. Two reasons, and the second is the one that matters.
//!
//! Today every node marion runs is a **bounded run** — `run_bounded`, `DuplexSpec::wall_clock`, a
//! contract's `timeout_secs` — so every file is bounded by its node's own lifetime. The unbounded
//! case arrives with the interactive surface, not before.
//!
//! But `snapshot.json` will never be the answer when it does. **The journal can be compacted
//! because it is a fold**: the tree is a small fixed state, so its prefix is genuinely redundant
//! and a snapshot preserves everything a replay would produce. **An event stream's whole value is
//! that it is not folded** — there is no smaller equivalent state, because the events themselves
//! are the product. So the eventual answer is size- or age-bounded *retention*, not compaction.
//!
//! And that answer needs a shape this workspace does not yet have: §7.4's truncation discipline —
//! and [`Truncation`], and [`EventLog::extend`]'s cursor rule — is **entirely about the tail**. A
//! head-truncated file, which is what retention produces, is a state neither can express, and a
//! reader that met one would silently report a partial stream as a whole one. Filed as §11 item 27.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::{AgentId, ExitStatus, ProcessExit};
use crate::encoding::SystemTime;
use crate::harness::Harness;
use crate::ir::{Completeness, EventId, Provenance, SrcSeq};
use crate::registry::Truncation;

/// The largest encoded event this file will carry, **newline included**.
///
/// Unlike `journal::MAX_RECORD_BYTES` this is **not** an atomicity bound — see the module doc.
/// There is one writer per file, so nothing about correctness depends on a single `write(2)`. What
/// this bounds is how large a line a reader must buffer to make progress, and how much one frame
/// may cost a stream a client is rendering incrementally.
///
/// **Measured, 2026-08-06, over every `*.jsonl` capture in `tests/fixtures/`** (s1, s4, s5, s6, s7,
/// s9–s15), unwrapping the `{t_rel,msg}` and `{dir,frame}` recording envelopes so what is compared
/// is the frame a reader would actually see:
///
/// | | bytes | what |
/// |---|---|---|
/// | largest frame in any capture | **5,145** | a Claude Code transcript `attachment` (s4) |
/// | largest frame on a *stdout stream* | **1,764** | a Claude Code `system` frame (s1, s4, s10) |
/// | largest frame **measured but not fixtured** | **~30,000** | the `initialize` `control_response` (S9) — the fixture *reduces* it, so the captures understate reality here |
///
/// So 256 KiB is roughly **8× the largest frame ever measured** and ~50× the largest one on disk.
///
/// **What is still not measured, and why the shortening path is not decoration.** A `ToolResult`
/// carrying a file's contents is bounded by the file, not by the harness, and no capture here
/// contains a large one. That case is genuinely unbounded, which is exactly why over-size is
/// [`bound`]'s honest shortening rather than a refusal: an under-estimate costs the body of one
/// event and never the event.
///
/// Note the largest measured frame is also one marion **must not keep** — §5.2 forbids journaling
/// the `initialize` reply verbatim, so it is recorded as [`Payload::Withheld`] and never approaches
/// this bound at all.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;

/// One line of `events.jsonl` — §4's `Event`, minus what marion cannot honestly fill in.
///
/// Four of §4's fields are absent, each a refusal rather than an omission:
///
/// * **`global_seq`** — §4.2 defines it as *"assigned by the supervisor on receipt"*, on the ground
///   that *"the supervisor is the only process receiving and writing normalized events — it **is**
///   the sequencer"*. That premise does not hold: a child's stream is read and written by the
///   **bridge's** process (`run_spawn`), which is not the supervisor and shares no counter with it.
///   A number written here would be a total order across processes that nothing assigns. §4.2's own
///   escape is already in the journal — *"the total order is the file's byte order"* — and this file
///   has one writer, so [`Self::agent_seq`] is a genuine total order over it.
/// * **`parent_id`** — the journal's, and immutable (§7.5). See the module doc.
/// * **`thread_id` / `turn_id` / `item_id`** — harness-native correlation, extractable only by the
///   same per-frame normalization [`Normalization`] refuses to fake. They land with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Carried on every event even though the file is per-node, for the reason the journal carries
    /// it on every record: an event **leaves** this file — §7.3.3 replays it to a client as the
    /// same notification a live subscribe would have delivered — and one that did not say which
    /// node it was about would have that fact re-attached at the boundary by whoever read the file.
    /// That is fabrication at exactly the seam this file exists to make honest.
    pub agent_id: AgentId,
    /// §4's per-agent observation order, from 0, **gapless by construction** — one writer owns it
    /// (`marion_supervisor::events`), and it is seeded from the file's own tail on open so a resume
    /// continues the sequence rather than restarting it.
    ///
    /// This is §7.3.3's seam value: *"replay to the journal's own read point, then subscribe from
    /// there"* is stated in ordinals, and a duplicate or a hole here is a duplicate or a gap in a
    /// client's rendering. [`EventLog`] reports both rather than repairing either.
    ///
    /// **It is observation order and proves nothing about completeness** (§4.2). A frame the
    /// harness emitted and marion never read simply never gets a number, leaving the sequence
    /// contiguous. Loss detection is [`Self::src_seq`]'s job, where the harness supplies one.
    pub agent_seq: u64,
    /// §4.2's source-side ordering evidence. `None` on Codex and on Claude Code `headless`, which
    /// is most of what marion runs today — and where it is `None`, **marion cannot detect loss and
    /// must not imply otherwise**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_seq: Option<SrcSeq>,
    /// §4's explicit causal edge. Present only where a source states one; never inferred from
    /// adjacency, which is the inference §4.2 rules out for counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<EventId>,
    /// §4/§6.7 encoding: RFC3339 UTC, literal `Z`, three fractional digits. **Display only** —
    /// §4.2 says NTP steps and sleep/wake move it backwards.
    pub ts: SystemTime,
    /// Monotonic since this writer started. §4.2's reason and no other: aligning this file with
    /// `pty.cast`, whose asciicast v3 timestamps are relative and otherwise unanchorable.
    pub mono_ns: u64,
    pub provenance: Provenance,
    pub payload: Payload,
}

impl Event {
    /// Whether this event must be durable before anything observable depends on it (§4.3).
    ///
    /// **Only the lifecycle bookends**, and the argument is §4.3's own: *"Losing trailing content
    /// deltas costs a slightly truncated replay; losing a lifecycle record costs an untracked live
    /// process. Only the latter pays for a barrier."* Here the cost of a lost bookend is sharper
    /// than a stale tree — a stream with no terminal event reads as a node that **stopped
    /// mid-turn**, which is §7.3.3's replay leg reporting the wrong thing about a finished node.
    /// A lost content frame costs one missing line in a transcript.
    pub fn is_barrier(&self) -> bool {
        matches!(self.payload, Payload::Lifecycle(_))
    }
}

/// What an event says. Externally tagged (`{"Vendor":{…}}`), matching the journal's `RecordKind`
/// and §6.7's `Workspace` — one tagging convention across marion's persisted JSON, not three.
///
/// **Additive by rule**, exactly as the journal is: a new variant may be added, an existing one may
/// only gain `#[serde(default)]` fields. An event written by an older marion must still read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Payload {
    /// The stream's bookends. See the module doc for why this file carries them at all when the
    /// journal already does.
    Lifecycle(Lifecycle),
    /// A harness frame, **carried verbatim**. §4.2: *"`Vendor` payloads are carried, never
    /// discarded"*, and that is the whole of what marion can honestly say about a frame it has not
    /// normalized. `key` is the harness's own discriminator for the frame — codex's `item.completed`,
    /// Claude Code's `type` — recorded as the harness spelled it, never translated.
    Vendor {
        harness: Harness,
        key: String,
        json: Value,
    },
    /// A line of the node's stdout that was not JSON. §3.4 already names `Payload::Raw` as what a
    /// node with no `ControlPlane` produces, and `duplex::StreamEvent::Unparsed` is the same
    /// finding one layer up: *"a stdout line that is not JSON is still something the node said"*.
    /// S12 recorded gemini interleaving `Warning: Basic terminal detected…` on stdout; a reader
    /// that dropped those would render a node that printed a stack trace as an unexplained silence.
    ///
    /// `String` and not §4's `Bytes`: the line reached here through a UTF-8 read, so bytes would be
    /// a re-encoding claiming a fidelity the source never had.
    Raw(String),
    /// §4's five **normalized** payloads — `Message`, `ToolCall`, `ToolResult`, `Permission`,
    /// `Usage` — and the reason none of them can be emitted yet.
    ///
    /// [`Normalization`] is uninhabited, so this variant cannot be constructed and cannot be
    /// decoded. That is the point: producing one means adding a variant there, which is the moment
    /// somebody has to write down the per-frame mapping from a harness's vocabulary to marion's.
    /// Until then every frame is [`Self::Vendor`], which is lossless and honest, rather than a
    /// normalized payload that is neither. See the module doc.
    Normalized(Normalization),
    /// A payload [`bound`] would not write whole. **Not a dropped event** — it keeps its ordinal
    /// and its provenance carries `Completeness::Partial`, which is the word §4.1 supplies for
    /// exactly this and the journal's vocabulary lacks. See the module doc.
    Oversized { was: PayloadKind, bytes: usize },
    /// A frame marion **read and deliberately did not keep**, because a normative rule forbids
    /// keeping it. Distinct from [`Self::Oversized`], which is a size accident, and from an event
    /// that was never written, which is an absence: this says *a frame occurred, marion saw it, and
    /// recording its body is not allowed*.
    ///
    /// The rule that creates this today is **§5.2's MUST**: *"A launcher that sends `initialize`
    /// MUST NOT journal or forward the reply verbatim, and SHOULD retain only the fields it
    /// actually reads."* S9 measured that reply at **~30 kB** carrying the operator's entire
    /// slash-command catalogue with descriptions, the subagent list, the model list **with prices**,
    /// `output_style`, `account.tokenSource` and the CLI's `pid` — machine- and account-specific
    /// data that marion reads exactly one field of. An events file is a journal in every sense the
    /// MUST cares about, so carrying that frame verbatim here would violate it as squarely as
    /// writing it to `journal.jsonl` would.
    ///
    /// `key` and `bytes` are what marion may keep: *that* the frame happened, what kind it was, and
    /// how large it was. `reason` names the rule, so a reader meeting one is not left guessing
    /// whether marion lost the body or withheld it.
    Withheld {
        key: String,
        bytes: usize,
        reason: String,
    },
}

impl Payload {
    pub fn kind(&self) -> PayloadKind {
        match self {
            Payload::Lifecycle(_) => PayloadKind::Lifecycle,
            Payload::Vendor { .. } => PayloadKind::Vendor,
            Payload::Raw(_) => PayloadKind::Raw,
            Payload::Normalized(n) => match *n {},
            Payload::Oversized { .. } => PayloadKind::Oversized,
            Payload::Withheld { .. } => PayloadKind::Withheld,
        }
    }
}

/// **Uninhabited on purpose.** See [`Payload::Normalized`] and the module doc: this is §4's
/// normalized payload set, refused in the type rather than in a comment, so the absence is
/// something a future implementer hits rather than reads past.
///
/// Adding the first variant is the commitment that a per-frame mapping exists and has been
/// measured against a capture. Nothing about that mapping is decided here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Normalization {}

/// The stream's bookends, and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Lifecycle {
    /// **marion began recording this node's stream here** — and that is a weaker claim than §4's
    /// `Lifecycle::Opened`, deliberately, which is why it is not called that.
    ///
    /// §4's `Spawned` asserts *a process exists*, carrying
    /// `{harness, harness_version, model, agent_type, isolation, caps, surfaces, depth}`. marion
    /// cannot truthfully make that claim at the moment this is written: a node's process is started
    /// **and reaped** inside one blocking call (`run_bounded`, `duplex::run_duplex`), so the first
    /// instant marion has observed a process existing is the instant it has already finished — which
    /// is precisely why `journal_the_roots_outcome` and `run_spawn` both write the journal's
    /// `Spawned` record *after* the call returns. Writing that claim here, before the call, would be
    /// the confirmation-of-something-that-has-not-happened §6.1's intent-then-confirm split exists
    /// to prevent.
    ///
    /// This is the claim marion *can* make at that instant, and it is the one a replay needs: the
    /// stream starts here. Its value is negative evidence — a file holding only this says **the node
    /// said nothing**, which is a different fact from a node nobody recorded
    /// ([`crate::event`]'s reader reports that as no file at all), and a file holding this and
    /// frames but no terminal event says the stream stopped mid-turn.
    ///
    /// **Carries nothing**, for the same one-fact-one-home reason: every field §4 lists is already
    /// in the journal's `SpawnIntent`/`Spawned` or belongs to `meta.json` (§4.3: *"compiled spec,
    /// caps, harness ref, binary path + version"* — declared and unwritten, see [`crate::paths`]),
    /// and a client attaching has its `NodeSummary` from the registry before it opens this file.
    /// Neither of the two is a reason to duplicate them *here*: this event's job is to say the
    /// stream started, and a field marion cannot source would be filled in with a guess.
    Opened,
    /// The stream ends here, with the terminal status marion observed.
    ///
    /// This one **does** carry its facts, for the reason `Spawned` does not: they are what
    /// distinguishes *"finished"* from *"stopped mid-turn"*, which is the single question a replay
    /// of a node the client never saw exists to answer. Sourcing it from the journal instead would
    /// make the replay leg depend on two files with two cursors, and the whole seam argument
    /// (§7.3.3, and `marion_supervisor::events`) is that there is exactly one cursor.
    Exited {
        status: ExitStatus,
        exit: ProcessExit,
    },
    /// The stream ends because **marion abandoned the run**, not because the node finished.
    ///
    /// The third terminal reading, and it exists for the reason [`Self::Exited`] carries its status:
    /// without it, a launch marion refused — a bridge that never came up, a worktree that could not
    /// be made — leaves a file holding [`Self::Opened`] and nothing else, which is the *same* shape
    /// as a node that started and was still mid-turn when everything stopped. Those are different
    /// facts and the operator's next move differs for each. Mirrors the journal's
    /// `RecordKind::SpawnAborted`, whose own doc gives the same argument: §7.2 is emphatic that a
    /// node marion decided the fate of must never be mistaken for one marion *lost*.
    Aborted { reason: String },
}

/// Which [`Payload`] a shortened event used to be. A tag rather than a string, so
/// [`Payload::Oversized`] cannot report a payload kind that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadKind {
    Lifecycle,
    Vendor,
    Raw,
    Normalized,
    Oversized,
    Withheld,
}

/// Encoding refused an event.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error(
        "an event of {0} bytes exceeds the {MAX_EVENT_BYTES}-byte line bound. Unlike a journal \
         record this is not a refusal the caller must live with: `event::bound` shortens the \
         payload to `Oversized` at `Completeness::Partial`, which keeps the event and its ordinal. \
         Reaching this error means `bound` was not applied."
    )]
    TooLarge(usize),
    #[error("serializing an event: {0}")]
    Json(#[from] serde_json::Error),
}

/// Shorten a payload this file will not carry whole — **the honest half of the size bound**.
///
/// Separate from [`encode`] and mandatory before it, rather than folded inside, because shortening
/// an event is a decision with a visible consequence (`Completeness::Partial`) and a caller should
/// be able to see it, test it, and report it. A codec that silently rewrote its input would make
/// "what marion wrote" differ from "what marion was asked to write" with nothing saying so — which
/// is the silent-failure shape §12 keeps recording.
///
/// The ordinal, the timestamps and the provenance's other axes are untouched. See the module doc
/// for why an event may be shortened at all when a journal record may not.
pub fn bound(mut event: Event) -> Event {
    let len = match serde_json::to_vec(&event) {
        Ok(v) => v.len() + 1,
        // Unserializable is not over-large, and pretending otherwise would report the wrong fault.
        // `encode` will surface the real error.
        Err(_) => return event,
    };
    if len <= MAX_EVENT_BYTES {
        return event;
    }
    let bytes = serde_json::to_vec(&event.payload)
        .map(|v| v.len())
        .unwrap_or(len);
    event.payload = Payload::Oversized {
        was: event.payload.kind(),
        bytes,
    };
    // §4.1's word for it. The UI keys on `completeness` before claiming anything about loss, so an
    // event whose body was dropped must not present as `Complete`.
    event.provenance.completeness = Completeness::Partial;
    event
}

/// One event as its line, newline included. **The only place an event becomes bytes.**
pub fn encode(event: &Event) -> Result<Vec<u8>, EncodeError> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    if line.len() > MAX_EVENT_BYTES {
        return Err(EncodeError::TooLarge(line.len()));
    }
    debug_assert!(
        line[..line.len() - 1].iter().all(|b| *b != b'\n'),
        "compact JSON escapes newlines; a raw one would forge a frame boundary"
    );
    Ok(line)
}

/// One line back to an event. `None` for anything that is not a complete, valid event — the fold
/// treats that as the end of the intact prefix.
pub fn decode(line: &[u8]) -> Option<Event> {
    serde_json::from_slice(line).ok()
}

/// §4.2's `Ordinal` loss, per node: an `agent_seq` that was written and is not in the file.
///
/// **Reported, never repaired.** A stream replayed over a hole is indistinguishable from a complete
/// one to anyone who only reads the events — the surviving ones still parse, still render, and
/// still end — so a reader that did not carry this separately would serve a partial stream as a
/// whole one. Same argument `Registry::boot_gaps` makes for the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventGap {
    pub agent_id: AgentId,
    pub expected: u64,
    pub found: u64,
}

/// Everything a reading of `events.jsonl` noticed **about the reading itself** — and nothing about
/// the events, which it hands to its caller.
///
/// **The deliberate difference from [`crate::registry::Replay`]**: `Replay` keeps the tree because
/// the journal is a *fold* onto a small fixed state. There is no fold here (the module doc says why
/// that is also why there is no compaction), so keeping the events would make this structure grow
/// without bound for the whole life of a node — the exact cost the growth section is about. So
/// [`Self::extend`] appends decoded events to a caller-supplied `Vec` and this keeps O(1) state:
/// the cursor's ordinal expectation, the counts, and what it could not read.
///
/// The state that *is* kept lives here rather than as locals of a one-shot function for the reason
/// `Replay` gives in as many words: a reader tailing a growing file is many `extend`s over the same
/// file's successive tails, and ordinal expectation reset per chunk would detect a lost event only
/// when the whole file happened to arrive in one read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventLog {
    /// Events accepted. Not lines in the file — the two differ by [`Self::truncation`].
    pub records: u64,
    /// The last source-side ordering evidence any event carried, §7.3.3's other seam value. An
    /// event without one never erases one that had it: absent evidence is not evidence of absence.
    pub last_src_seq: Option<SrcSeq>,
    /// Where **this** `extend` stopped, or `None` if it consumed everything offered.
    pub truncation: Option<Truncation>,
    /// §4.2's loss, in the one form a per-node stream can supply. See [`EventGap`].
    pub gaps: Vec<EventGap>,
    /// Per node, the `agent_seq` the next event is expected to carry. A map and not a scalar
    /// because an event names its own node and this refuses to assume the file holds only one —
    /// asserting that would make a misfiled event read as a gap, which is a different fault.
    expected: HashMap<String, u64>,
}

impl EventLog {
    /// Fold `bytes` — the whole file, or the tail past a cursor — appending every event read to
    /// `out`, and answer how many **bytes** were consumed.
    ///
    /// The cursor rule is `Replay::extend`'s, and it is the same rule for the same reason: consume
    /// only the intact prefix, so a caller that re-offers from the returned offset reads a torn
    /// record exactly once, when it completes. The writer is another process appending, so meeting
    /// a partial line is the normal case rather than an error (§7.4).
    ///
    /// **Total and infallible.** Every byte string is a valid input, including an empty one, one
    /// that is not UTF-8, and one cut mid-record. There is no error return because there is no
    /// failure mode a caller could act on differently.
    pub fn extend(&mut self, bytes: &[u8], out: &mut Vec<Event>) -> usize {
        self.truncation = None;
        let mut offset = 0usize;
        for (line_no, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            if line.last() != Some(&b'\n') {
                if !line.is_empty() {
                    self.truncation = Some(Truncation::UnterminatedTail {
                        byte_offset: offset,
                        bytes: line.len(),
                    });
                }
                break;
            }
            let body = &line[..line.len() - 1];
            if !body.is_empty() {
                match decode(body) {
                    Some(event) => {
                        self.check_seq(&event);
                        if event.src_seq.is_some() {
                            self.last_src_seq = event.src_seq.clone();
                        }
                        self.records += 1;
                        out.push(event);
                    }
                    None => {
                        // A complete line marion cannot read. On an append-only file that is
                        // corruption rather than a torn write, and narrating past it would render a
                        // stream from bytes marion does not understand.
                        self.truncation = Some(Truncation::Unparsable {
                            byte_offset: offset,
                            line: line_no,
                        });
                        break;
                    }
                }
            }
            offset += line.len();
        }
        offset
    }

    /// The `agent_seq` a writer appending to this file must use next, or `None` for a file with no
    /// events. **This is what makes a resume continue the sequence rather than restart it at 0**,
    /// which would put two events with one ordinal in one file and make the seam ambiguous.
    ///
    /// The maximum across nodes, not per node, because the writer owns the file rather than a node
    /// within it — and a file holding two nodes is already a fault this cannot repair.
    pub fn next_seq(&self) -> Option<u64> {
        self.expected.values().copied().max()
    }

    fn check_seq(&mut self, event: &Event) {
        let next = event.agent_seq.saturating_add(1);
        match self.expected.insert(event.agent_id.0.clone(), next) {
            Some(expected) if expected != event.agent_seq => self.gaps.push(EventGap {
                agent_id: event.agent_id.clone(),
                expected,
                found: event.agent_seq,
            }),
            _ => {}
        }
    }
}

/// Read a whole file's bytes into the events they record, plus the reading's own account of itself.
///
/// One [`EventLog::extend`]. A reader tailing a growing file calls `extend` repeatedly instead and
/// gets the same value — the property that lets a replay and a live subscription be **one cursor**
/// rather than two, which is the whole of §7.3.3's seam.
pub fn read(bytes: &[u8]) -> (EventLog, Vec<Event>) {
    let mut log = EventLog::default();
    let mut out = Vec::new();
    log.extend(bytes, &mut out);
    (log, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{AgentId, ExitStatus, ProcessExit};
    use crate::encoding::SystemTime;
    use crate::harness::Harness;
    use crate::ir::{Completeness, EventId, Provenance, Source, SrcSeq};

    fn ev(seq: u64, payload: Payload) -> Event {
        Event {
            agent_id: AgentId("a-1".into()),
            agent_seq: seq,
            src_seq: None,
            caused_by: None,
            ts: SystemTime::from_unix_millis(1_785_625_628_619),
            mono_ns: 42,
            provenance: Provenance::marion(),
            payload,
        }
    }

    fn frame() -> Payload {
        Payload::Vendor {
            harness: Harness::Codex,
            key: "item.completed".into(),
            json: serde_json::json!({"item": {"id": "msg_09cb"}}),
        }
    }

    #[test]
    fn an_event_pins_its_wire_shape() {
        assert_eq!(
            serde_json::to_value(ev(0, Payload::Lifecycle(Lifecycle::Opened))).unwrap(),
            serde_json::json!({
                "agent_id": "a-1",
                "agent_seq": 0,
                "ts": "2026-08-01T23:07:08.619Z",
                "mono_ns": 42,
                "provenance": {
                    "source": "Marion",
                    "source_id": null,
                    "observed_live": true,
                    "authoritative": true,
                    "completeness": "Complete",
                    "transformation": "Native",
                },
                "payload": {"Lifecycle": "Opened"},
            })
        );
    }

    #[test]
    fn every_payload_round_trips_through_a_line() {
        let payloads = [
            Payload::Lifecycle(Lifecycle::Opened),
            Payload::Lifecycle(Lifecycle::Exited {
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "clean exit".into(),
                },
            }),
            frame(),
            Payload::Raw("Warning: Basic terminal detected".into()),
            Payload::Oversized {
                was: PayloadKind::Vendor,
                bytes: 900_000,
            },
            Payload::Withheld {
                key: "control_response".into(),
                bytes: 30_000,
                reason: "§5.2".into(),
            },
        ];
        for (i, p) in payloads.into_iter().enumerate() {
            let e = ev(i as u64, p);
            let line = encode(&e).unwrap();
            assert_eq!(line.last(), Some(&b'\n'));
            assert_eq!(decode(&line[..line.len() - 1]).as_ref(), Some(&e));
        }
    }

    /// §5.2's MUST, in the type: *"A launcher that sends `initialize` MUST NOT journal or forward
    /// the reply verbatim, and SHOULD retain only the fields it actually reads."*
    #[test]
    fn a_withheld_payload_records_that_a_frame_existed_without_keeping_its_body() {
        let e = ev(
            3,
            Payload::Withheld {
                key: "control_response".into(),
                bytes: 30_000,
                reason: "§5.2: the initialize reply is the session catalogue".into(),
            },
        );
        let line = encode(&e).unwrap();
        assert!(
            line.len() < 500,
            "the point is that the body is not here: {} bytes",
            line.len()
        );
        assert_eq!(decode(&line[..line.len() - 1]).as_ref(), Some(&e));
        assert_eq!(e.payload.kind(), PayloadKind::Withheld);
    }

    /// The refusal §4's five normalized payloads are under, **enforced by the type**: there is no
    /// value of [`Normalization`], so no code path can produce one and no line can decode to one.
    #[test]
    fn a_normalized_payload_cannot_be_produced_or_decoded_until_someone_builds_one() {
        let line = br#"{"agent_id":"a-1","agent_seq":0,"ts":"2026-08-01T23:07:08.619Z","mono_ns":1,"provenance":{"source":"Protocol"},"payload":{"Normalized":{"Message":{}}}}"#;
        assert!(
            decode(line).is_none(),
            "no wire form may conjure a normalized payload while `Normalization` is uninhabited"
        );
    }

    #[test]
    fn an_absent_src_seq_is_absent_from_the_wire_not_null() {
        let json = serde_json::to_string(&ev(0, frame())).unwrap();
        assert!(!json.contains("src_seq"), "{json}");
        let with = Event {
            src_seq: Some(SrcSeq::Predecessor(EventId("u-9".into()))),
            ..ev(0, frame())
        };
        assert!(
            serde_json::to_string(&with)
                .unwrap()
                .contains(r#""src_seq":{"Predecessor":"u-9"}"#)
        );
    }

    #[test]
    fn an_older_event_still_deserializes() {
        let old = br#"{"agent_id":"a-1","agent_seq":7,"ts":"2026-08-01T23:07:08.619Z","mono_ns":1,
            "provenance":{"source":"Protocol"},"payload":{"Raw":"hello"}}"#;
        let compact: Vec<u8> = old.iter().copied().filter(|b| *b != b'\n').collect();
        let e = decode(&compact).expect("an older event must still read");
        assert_eq!(e.src_seq, None);
        assert_eq!(e.caused_by, None);
        assert_eq!(e.provenance.completeness, Completeness::Unknown);
    }

    #[test]
    fn a_newline_in_a_payload_is_escaped_and_never_forges_a_frame() {
        let e = ev(0, Payload::Raw("line one\nline two\r\n".into()));
        let line = encode(&e).unwrap();
        assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
        assert_eq!(decode(&line[..line.len() - 1]).as_ref(), Some(&e));
    }

    #[test]
    fn decode_rejects_rather_than_guesses() {
        assert!(decode(b"").is_none());
        assert!(decode(b"{\"agent_id\":\"a\"").is_none(), "a torn prefix");
        assert!(decode(b"not json").is_none());
        assert!(
            decode(&[0xff, 0xfe]).is_none(),
            "invalid UTF-8 must not panic"
        );
    }

    /// The journal refuses an over-large record; this file **shortens** and says so. The event
    /// keeps its ordinal, which is what the seam rests on.
    #[test]
    fn an_oversized_payload_is_shortened_and_says_so_rather_than_being_dropped() {
        let huge = ev(5, Payload::Raw("x".repeat(MAX_EVENT_BYTES + 1_000)));
        assert!(encode(&huge).is_err(), "unbounded, the codec still refuses");

        let b = bound(huge);
        assert_eq!(
            b.agent_seq, 5,
            "the ordinal survives; the seam depends on it"
        );
        assert_eq!(b.provenance.completeness, Completeness::Partial);
        match b.payload {
            Payload::Oversized { was, bytes } => {
                assert_eq!(was, PayloadKind::Raw);
                assert!(bytes > MAX_EVENT_BYTES, "{bytes}");
            }
            other => panic!("{other:?}"),
        }
        assert!(encode(&bound(ev(5, frame()))).is_ok());
        assert_eq!(
            bound(ev(5, frame())).payload,
            frame(),
            "a payload that fits is untouched"
        );
    }

    fn lines(events: &[Event]) -> Vec<u8> {
        events.iter().flat_map(|e| encode(e).unwrap()).collect()
    }

    fn stream() -> Vec<Event> {
        (0..4)
            .map(|i| {
                let mut e = ev(i, frame());
                e.provenance.source = Source::Protocol;
                e
            })
            .collect()
    }

    #[test]
    fn a_fold_hands_over_its_events_rather_than_accumulating_them() {
        let mut log = EventLog::default();
        let mut out = Vec::new();
        let n = log.extend(&lines(&stream()), &mut out);
        assert_eq!(n, lines(&stream()).len());
        assert_eq!(out, stream());
        assert_eq!(log.records, 4);
        assert_eq!(log.next_seq(), Some(4), "where the next event must start");
        assert!(log.gaps.is_empty());
    }

    #[test]
    fn extend_consumes_only_the_intact_prefix_so_a_torn_tail_is_read_once_when_it_completes() {
        let whole = lines(&stream());
        let cut = whole.len() - 12;
        let mut log = EventLog::default();
        let mut out = Vec::new();
        let n = log.extend(&whole[..cut], &mut out);
        assert!(
            matches!(log.truncation, Some(Truncation::UnterminatedTail { .. })),
            "{:?}",
            log.truncation
        );
        assert!(n < cut, "the torn tail is not consumed");
        assert_eq!(out.len(), 3);

        let n2 = log.extend(&whole[n..], &mut out);
        assert_eq!(n + n2, whole.len());
        assert_eq!(log.truncation, None);
        assert_eq!(out, stream(), "the fourth event arrives exactly once");
    }

    /// §4.2's `Ordinal` loss, per node. Reported, never repaired — a stream replayed over a hole
    /// looks exactly like a complete one to anyone who only reads the events.
    #[test]
    fn a_gap_in_agent_seq_is_reported_and_the_events_are_still_delivered() {
        let mut s = stream();
        s.remove(2);
        let mut log = EventLog::default();
        let mut out = Vec::new();
        log.extend(&lines(&s), &mut out);
        assert_eq!(out.len(), 3, "what survived is still served");
        assert_eq!(
            log.gaps,
            vec![EventGap {
                agent_id: AgentId("a-1".into()),
                expected: 2,
                found: 3
            }]
        );
    }

    #[test]
    fn an_unparsable_line_stops_the_fold_rather_than_being_skipped() {
        let mut bytes = lines(&stream()[..2]);
        bytes.extend_from_slice(b"{\"agent_id\":\"a-1\"}\n");
        bytes.extend_from_slice(&lines(&stream()[2..]));
        let mut log = EventLog::default();
        let mut out = Vec::new();
        log.extend(&bytes, &mut out);
        assert_eq!(out.len(), 2, "nothing past bytes marion cannot read");
        assert!(matches!(
            log.truncation,
            Some(Truncation::Unparsable { .. })
        ));
    }

    #[test]
    fn only_a_lifecycle_bookend_is_a_barrier() {
        assert!(ev(0, Payload::Lifecycle(Lifecycle::Opened)).is_barrier());
        assert!(!ev(0, frame()).is_barrier());
        assert!(!ev(0, Payload::Raw("x".into())).is_barrier());
    }
}
