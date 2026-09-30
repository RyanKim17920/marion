//! `events.jsonl`, **the file** (design §4, §7.3.3) — `marion-core` owns the events, the codec and
//! the fold; this owns the bytes on disk and the cursor over them.
//!
//! ```text
//! <state>/<project-hash>/agents/<agent_id>/events.jsonl      # IR, append-only
//! ```
//!
//! # One writer per file, and why that is the opposite conclusion to the journal's
//!
//! `journal.rs` opens its concurrency argument with the fact that `marion run` and each
//! `marion-supervisor mcp` bridge are **separate processes**, and concludes that the journal has
//! concurrent writers by construction — hence `O_APPEND` atomicity under a 16 KiB cap, in place of
//! a lock. The same fact leads the other way here, and it is worth stating why rather than
//! inheriting the discipline out of symmetry.
//!
//! The journal is **one file for the project**, so every process that causes a lifecycle event
//! writes it. `events.jsonl` is **one file per agent dir**, an agent dir belongs to exactly one
//! node, and a node is driven by exactly one process: `run::run_spawn` for a child — which is the
//! bridge's process, and `registry.rs` is emphatic that this is where a child's whole life happens
//! — and `root::launch` for a root. A resume opens the file again *after* the first run terminated.
//! Sequential, never concurrent. So there is no interleaving to defend against, and
//! `marion_core::event::MAX_EVENT_BYTES` is a line-length bound rather than an atomicity one
//! (`event.rs` gives the deeper reason the journal's cap must not be copied here: a journal record
//! cannot be shortened, an event can, because `Completeness::Partial` is the word for it).
//!
//! **That invariant is checked rather than assumed.** [`EventWriter::open_path`] seeds `agent_seq`
//! from the file's own tail, so a resume continues the sequence — and a *second* writer against one
//! file therefore produces duplicate ordinals that
//! `marion_core::event::EventLog` reports, rather than a stream that reads as plausible. A property
//! true only by construction stays true only until somebody changes the construction, so
//! `a_second_writer_on_one_file_is_visible_as_duplicate_ordinals_not_a_plausible_stream` is the
//! test that notices. `O_APPEND` is kept for the same reason it costs nothing: it makes that case
//! confusing rather than *torn*.
//!
//! # Bounded on disk: one rotation
//!
//! A node that runs for days would otherwise grow its stream without limit. Past
//! [`MAX_EVENTS_FILE_BYTES`] the writer renames the file to `events.jsonl.1` (replacing an older
//! one) and continues in a fresh `events.jsonl`, the ordinals running on; so a node holds at most
//! two caps on disk. A replay reads the rotated half and then the current file; a follower that
//! sees a new inode at the path finishes the old file from its rotated name and moves on, and one
//! that fell two rotations behind reports the ordinals it skipped as a gap.
//!
//! # §7.3.3's seam is a cursor, not a lock
//!
//! `handler.rs` splices `tree/subscribe`'s snapshot to its notifications by taking **one** registry
//! read under **one** lock, so nothing can slip between building the snapshot and recording it.
//! That shape does not transfer here, and the reason is structural: the writer is **in another
//! process**, and two processes share no lock. There is nothing to take.
//!
//! The answer is `registry.rs`'s, one level down. It tails `journal.jsonl` rather than being updated
//! where marion emits, on the ground that a registry updated in-process *"would be unable to see"*
//! the other process's nodes at all — *"tailing is not the slower correct option, it is the only
//! one"*. Apply that here and **the seam stops existing**: a live subscription is itself a tail of
//! `events.jsonl`, so replay and subscribe are not two mechanisms to splice but **one
//! [`EventReader`] cursor**, and a gap or a duplicate at the join is unreachable because there is no
//! join. §7.3.3's *"replay to the journal's own read point, then subscribe from there"* becomes
//! literally true rather than a procedure someone has to implement correctly.
//!
//! The latency argument is the one `registry.rs` already made and measured against its own barrier
//! test: §4.3's ~50 ms group commit delays the **fsync**, not the `write(2)`, so the bytes are
//! visible to another process the moment [`EventWriter::append`] returns. What a follower pays is
//! its poll interval.
//!
//! # A viewer may never affect the run
//!
//! [`EventReader`] follows `watch.rs`'s policy exactly, including its one refusal: a torn tail is
//! re-read next poll (the writer is another process appending — meeting a partial line is normal),
//! and a **complete line that is not an event stops the reader for good**, because an append-only
//! file cannot heal a bad line and a reader that narrated past bytes it does not understand would
//! be inventing a stream.

use std::cell::RefCell;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use marion_core::contract::{AgentId, TokenUsage};
use marion_core::encoding::SystemTime;
use marion_core::event::{EncodeError, Event, EventLog, Lifecycle, Payload, bound, encode};
use marion_core::harness::Harness;
use marion_core::ir::{Completeness, EventId, Provenance, Source, SrcSeq, Transformation};
use marion_core::paths::AgentDir;
use marion_core::registry::Truncation;
use marion_harness::adapter::adapter_for;
use marion_harness::grammar::UsageMeter;

use crate::duplex::StreamEvent;
use crate::journal::GROUP_COMMIT_INTERVAL;

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("events.jsonl io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// **Everything about an event that only its source can answer.**
///
/// The split is the point. A caller supplies the payload and the provenance — what was said, and
/// what marion knows about how it came to know it. The *writer* supplies `agent_id`, `agent_seq`,
/// `ts` and `mono_ns`, and a caller cannot set them, because every one of them is a claim about the
/// file rather than about the node: an ordinal chosen by a caller is the duplicate this module's
/// whole seam argument rests on not happening.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub payload: Payload,
    pub provenance: Provenance,
    pub src_seq: Option<SrcSeq>,
    pub caused_by: Option<EventId>,
}

impl Draft {
    /// marion's own act, observed by marion as it happened — the lifecycle bookends and nothing
    /// else. Same `Provenance::marion()` the journal stamps on every record, and for the same
    /// reason: for *"marion observed this node start"* there is no other system of record.
    pub fn marion(payload: Payload) -> Self {
        Self {
            payload,
            provenance: Provenance::marion(),
            src_seq: None,
            caused_by: None,
        }
    }

    /// Something the **node** said, read live off its own surface.
    ///
    /// `authoritative: false`, deliberately. §4.1 defines the axis as *"is this the harness's system
    /// of record"*, and it is not: the harness's system of record is its own transcript, which §4.1
    /// notes is *"more complete after process death"* than an ephemeral notification stream. marion
    /// is reading a pipe. Marking these authoritative would make reconciliation prefer the weaker
    /// copy the day a transcript reader lands.
    ///
    /// `completeness: Complete` because a frame is a whole frame — the splitter guarantees it
    /// (`marion_harness::stream`) — and `Partial` is reserved for what [`bound`] shortens.
    pub fn observed(payload: Payload, source: Source) -> Self {
        Self {
            payload,
            provenance: Provenance {
                source,
                source_id: None,
                observed_live: true,
                authoritative: false,
                completeness: Completeness::Complete,
                transformation: Transformation::Native,
            },
            src_seq: None,
            caused_by: None,
        }
    }
}

/// What the duplex driver already observes, as what this file records — **the whole of the
/// mapping, in one place**.
///
/// `duplex::StreamEvent` has two variants for a measured reason (*"a stdout line that is not JSON
/// is still something the node said"*, after S12 recorded gemini interleaving `[STARTUP] Phase …`
/// on stdout), and both are carried: a frame becomes `Vendor`, a non-JSON line becomes `Raw`.
/// Neither is normalized, because nothing in this workspace can normalize one — see
/// `marion_core::event::Normalization`.
///
/// **`key` is the frame's `type`, verbatim — a JSON-RPC notification's `method` — or empty.**
/// Measured rather than assumed: `type` is the discriminator on every frame in `tests/fixtures/s1/`
/// (Claude Code `stream-json`), `s6/` (`codex exec --json`: `thread.started`, `item.completed`, …)
/// and S12's gemini capture, and `method` is the one on every JSON-RPC notification (S21's ACP
/// `session/update`, S36's app-server `item/completed`). A frame that carries neither — a JSON-RPC
/// answer — gets an **empty** key rather than an invented one: the whole frame is still in `json`,
/// so nothing is lost, and guessing a discriminator would be the fabrication
/// `handler/summary.rs`'s `Unprojectable` refuses.
pub fn from_stream_event(harness: Harness, ev: StreamEvent<'_>) -> Draft {
    let payload = match ev {
        StreamEvent::Frame(json) => Payload::Vendor {
            harness: harness.into(),
            key: frame_key(json).to_string(),
            json: json.clone(),
        },
        StreamEvent::Unparsed(line) => Payload::Raw(line.to_string()),
    };
    Draft::observed(payload, Source::Protocol)
}

/// **A node's stream, being recorded** — an [`EventWriter`] plus the two facts the mapping needs,
/// and the thing a launcher actually holds.
///
/// Exists rather than having call sites drive the writer directly because it owns the one rule that
/// must not be re-derived per call site: §5.2's MUST about the `initialize` reply (see
/// [`Self::record`]). A launcher that built drafts itself would have to remember it, and
/// `root::launch` and `run::run_spawn` are exactly the two places this document warns about drifting
/// apart.
///
/// **Interior mutability, because `duplex::StreamSink` is `&dyn Fn`.** That module already states
/// the contract — *"a sink that needs state carries its own `Cell`/`RefCell`"* — and this is that
/// sink. Nothing here is `Send`; it is called on the driver's own reader thread, between frames.
pub struct EventSink {
    writer: RefCell<EventWriter>,
    harness: Harness,
    /// The `request_id` this node's `initialize` went out with. What separates the one
    /// `control_response` §5.2 forbids keeping from every other one, which must be kept.
    init_id: String,
    /// An endpoint node's keys — every one it has been launched on — replaced by `***` in every
    /// event before it is written ([`Self::scrub_key`]); empty on a canned or live node.
    scrub: RefCell<Vec<String>>,
    /// **What the node's stream says it spent, folded as each frame is recorded.** Every frame of
    /// every path passes through this sink, so the node's usage costs no second read of its stream
    /// or its capture, and a capture cut short does not cut it short. `None` for a harness whose
    /// row states no usage rule.
    usage: RefCell<Option<UsageMeter<'static>>>,
    /// The row's rule for the model its stream says is running, and the latest model it named —
    /// read as each frame is recorded, for the reason [`Self::usage`] is.
    model: (
        Option<&'static marion_harness::grammar::ModelName>,
        RefCell<Option<String>>,
    ),
    /// Where the run's figure is published as it moves ([`Self::publishing_to`]).
    publish: Option<(AgentId, std::sync::Arc<crate::spending::Spending>)>,
}

impl EventSink {
    /// Open a node's stream for recording, or **`None` and a warning** if it cannot be opened.
    ///
    /// The failure policy, stated once, for the reason `journal::record` states its own: a viewer
    /// may never fail a run. A full disk must not kill a real child mid-edit, and recording is not
    /// something any decision depends on — nothing reads this file to decide whether a process may
    /// be spawned.
    ///
    /// **`None` is not silently equivalent to a node that produced nothing**, which is why this is
    /// an `Option` a caller can see rather than a no-op writer: no file is written at all, and
    /// [`EventReader::ever_written`] reports that as the distinct fact it is.
    pub fn open(
        agent: &AgentDir,
        agent_id: &AgentId,
        harness: Harness,
        init_id: String,
    ) -> Option<Self> {
        match EventWriter::open(agent, agent_id) {
            Ok(w) => Some(Self::new(w, harness, init_id)),
            Err(e) => {
                eprintln!(
                    "marion: cannot record {}'s events to {}: {e}",
                    agent_id.0,
                    agent.events().display()
                );
                None
            }
        }
    }

    /// The usage rule is the harness's own (`HarnessAdapter::usage_rule`), taken here so no
    /// launcher can forget it — the same argument as §5.2's rule in [`Self::record`].
    pub fn new(writer: EventWriter, harness: Harness, init_id: String) -> Self {
        let adapter = adapter_for(harness).ok();
        let usage = adapter
            .as_ref()
            .and_then(|a| a.usage_rule())
            .map(UsageMeter::new);
        let model_rule = adapter.as_ref().and_then(|a| a.model_rule());
        Self {
            writer: RefCell::new(writer),
            harness,
            init_id,
            scrub: RefCell::new(Vec::new()),
            usage: RefCell::new(usage),
            model: (model_rule, RefCell::new(None)),
            publish: None,
        }
    }

    /// Scrub `key` from every event this sink records — an endpoint node's, whose harness may echo
    /// its credential in an error frame while the run is live. The rule is
    /// [`crate::endpoint::redact`]'s, so a short key is left alone as it is everywhere else.
    pub fn scrubbing(self, key: Option<&str>) -> Self {
        if let Some(k) = key {
            self.scrub_key(k);
        }
        self
    }

    /// Add a key to scrub, from now on — a rotated node's next credential.
    pub fn scrub_key(&self, key: &str) {
        let mut keys = self.scrub.borrow_mut();
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
        }
    }

    /// This run resumes a node whose earlier runs recorded spending `prior`. See
    /// [`UsageMeter::resumed_from`].
    pub fn resumed_from(&self, prior: Option<TokenUsage>) {
        let mut usage = self.usage.borrow_mut();
        *usage = usage.take().map(|m| m.resumed_from(prior));
    }

    /// Publish the run's figure to `spending` each time a frame moves it, under `agent_id` — the
    /// live half of the one path from a usage frame to a figure (see [`crate::spending`]). `None`
    /// for an owner that keeps no live figures.
    pub fn publishing_to(
        mut self,
        agent_id: &AgentId,
        spending: Option<std::sync::Arc<crate::spending::Spending>>,
    ) -> Self {
        self.publish = spending.map(|s| (agent_id.clone(), s));
        self
    }

    /// The process this sink was recording has ended: what it spent is settled, and the next
    /// frame is the next generation's (a continuation). See [`UsageMeter`].
    pub fn end_generation(&self) {
        if let Some(m) = self.usage.borrow_mut().as_mut() {
            m.end_generation();
        }
    }

    /// Everything the recorded stream says this run spent so far, across its generations, and each
    /// turn's spend. A `None` usage when it stated nothing, which is not zero.
    pub fn spent(&self) -> crate::spending::Spent {
        self.usage
            .borrow()
            .as_ref()
            .map(|m| crate::spending::Spent {
                usage: m.usage(),
                turns: m.turns(),
            })
            .unwrap_or_default()
    }

    /// The model the recorded stream last said was running, or `None` where it named none (or
    /// the row reads no model).
    pub fn model(&self) -> Option<String> {
        self.model.1.borrow().clone()
    }

    /// Record one live [`StreamEvent`] — the body of the `duplex::DuplexSpec::sink` closure.
    ///
    /// **Best-effort by policy**: a viewer may never affect the run, so an I/O fault is loud on
    /// stderr and the node keeps going. Same argument as `EventWriter::record` and
    /// `journal::record`.
    ///
    /// **§5.2's MUST is applied here and nowhere else.** A `control_response` to *this node's*
    /// `initialize` is recorded as `Payload::Withheld` — the fact, its size, and the rule — and its
    /// body never reaches disk. Narrowly, on the request id: S11's interrupt reply is a
    /// `control_response` too, it is 100 bytes of `{"still_queued":[]}`, and it is precisely what a
    /// re-attaching client wants to see. Withholding by frame *type* would discard it.
    pub fn record(&self, ev: StreamEvent<'_>) {
        let draft = self.draft(ev, true);
        self.writer.borrow_mut().record(draft);
    }

    /// Record a whole captured stdout **after the fact** — the `LaunchOnly` path, which has no live
    /// seam to hook: `run_bounded` drains the pipe whole and hands back a `String`.
    ///
    /// Every event is `observed_live: false`, which is not a lesser recording but a true one. §4.1
    /// keeps that axis separate from `completeness` for this reason: a capture read after the
    /// process died is *exact* and *not live*, and those are different claims.
    ///
    /// Splitting is `marion_harness::stream::json_frames`, the same splitter every adapter reads
    /// with, so a line this records as `Raw` is exactly a line the adapter also failed to parse.
    pub fn record_capture(&mut self, stdout: &str) {
        for line in stdout.lines() {
            self.record_text_line(line, false);
        }
    }

    /// Record one stdout line **as it lands** — the `LaunchOnly` path's live seam
    /// (`run::run_bounded_watched`'s `on_line`). The same line rule as [`Self::record_capture`],
    /// `observed_live: true`, so a running node's file already says what it has done — which is
    /// what `status`'s peek at a running child reads. A path that records live never also records
    /// the capture, or every frame would be written twice.
    pub fn record_line(&self, line: &str) {
        self.record_text_line(line, true);
    }

    /// One stdout line as a frame when it is a JSON object, else as the raw line; a blank line is
    /// nothing.
    fn record_text_line(&self, line: &str, live: bool) {
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            return;
        }
        let draft = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) if v.is_object() => self.draft(StreamEvent::Frame(&v), live),
            _ => self.draft(StreamEvent::Unparsed(line), live),
        };
        self.writer.borrow_mut().record(draft);
    }

    /// A lifecycle bookend. Barrier-fsynced before it returns (`Event::is_barrier`), because a lost
    /// terminal bookend makes a finished node read as one that stopped mid-turn.
    pub fn lifecycle(&self, l: Lifecycle) {
        self.writer
            .borrow_mut()
            .record(Draft::marion(Payload::Lifecycle(l)));
    }

    pub fn next_seq(&self) -> u64 {
        self.writer.borrow().next_seq()
    }

    fn draft(&self, ev: StreamEvent<'_>, live: bool) -> Draft {
        let moved = match (&ev, self.usage.borrow_mut().as_mut()) {
            (StreamEvent::Frame(json), Some(m)) => m.observe(json),
            _ => false,
        };
        if let (StreamEvent::Frame(json), Some(rule)) = (&ev, self.model.0)
            && let Some(name) = marion_harness::grammar::model_in(rule, json)
        {
            *self.model.1.borrow_mut() = Some(name);
        }
        if let (true, Some((id, spending))) = (moved, &self.publish) {
            spending.publish(id, self.spent());
        }
        let mut d = match ev {
            StreamEvent::Frame(json) if self.is_own_initialize_reply(json) => {
                let bytes = serde_json::to_vec(json).map(|v| v.len()).unwrap_or(0);
                let mut d = Draft::observed(
                    Payload::Withheld {
                        key: frame_key(json).to_string(),
                        bytes,
                        reason: "§5.2: a launcher that sends `initialize` MUST NOT journal the \
                                 reply verbatim — it is the session catalogue (S9: ~30 kB of \
                                 commands, model prices, account.tokenSource and the CLI's pid)"
                            .into(),
                    },
                    Source::Protocol,
                );
                // The body is absent by rule, which is still absent: §4.1's word for having part of
                // a thing, and what stops a reader presenting this as the whole frame.
                d.provenance.completeness = Completeness::Partial;
                d
            }
            ev if self.scrub.borrow().is_empty() => from_stream_event(self.harness, ev),
            ev => self.scrubbed(ev),
        };
        d.provenance.observed_live = live;
        d
    }

    /// `ev` recorded with `key` replaced: a raw line directly, a frame through its text — re-parsed,
    /// so it is still recorded as the frame it was.
    fn scrubbed(&self, ev: StreamEvent<'_>) -> Draft {
        let redact = |text: &str| {
            self.scrub
                .borrow()
                .iter()
                .fold(text.to_string(), |t, k| crate::endpoint::redact(&t, k))
        };
        match ev {
            StreamEvent::Unparsed(line) => {
                from_stream_event(self.harness, StreamEvent::Unparsed(&redact(line)))
            }
            StreamEvent::Frame(json) => {
                let text = json.to_string();
                let clean = redact(&text);
                if clean == text {
                    return from_stream_event(self.harness, ev);
                }
                match serde_json::from_str::<serde_json::Value>(&clean) {
                    Ok(v) => from_stream_event(self.harness, StreamEvent::Frame(&v)),
                    Err(_) => from_stream_event(self.harness, StreamEvent::Unparsed(&clean)),
                }
            }
        }
    }

    /// Whether a frame is the `control_response` to **this node's** `initialize`.
    ///
    /// Reuses `duplex::is_control_response_to`, which is the same predicate the driver waits on, so
    /// the frame withheld here is exactly the frame the driver identified as its own reply. A second
    /// spelling of this shape could match a different set.
    fn is_own_initialize_reply(&self, frame: &serde_json::Value) -> bool {
        crate::duplex::is_control_response_to(frame, &self.init_id)
    }
}

/// A frame's own discriminator, or `""`. See [`from_stream_event`] for why it is `type` and why an
/// absent one is not invented.
fn frame_key(json: &serde_json::Value) -> &str {
    ["type", "method"]
        .iter()
        .find_map(|k| json.get(*k).and_then(serde_json::Value::as_str))
        .unwrap_or_default()
}

/// **The most one `events.jsonl` holds before it rotates** into `events.jsonl.1`, replacing an
/// older one: a node's stream is at most two of these on disk, however long it runs.
pub const MAX_EVENTS_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Where `path`'s rotated half lives: `events.jsonl.1` beside `events.jsonl`.
pub fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// An open `events.jsonl` for one node, with the next ordinal it will use.
pub struct EventWriter {
    path: PathBuf,
    file: File,
    /// Bytes in the current file, and the most it may hold ([`MAX_EVENTS_FILE_BYTES`]).
    len: u64,
    cap: u64,
    agent_id: AgentId,
    seq: u64,
    /// Bytes are in the page cache and not yet on disk.
    dirty: bool,
    last_sync: Instant,
    /// §4.2's `mono_ns` origin: monotonic since this writer started.
    start: Instant,
}

impl EventWriter {
    /// Open (creating) a node's event stream inside its agent dir.
    pub fn open(agent: &AgentDir, agent_id: &AgentId) -> Result<Self, EventError> {
        crate::private_fs::create_dir_all(agent.path())?;
        Self::open_path(&agent.events(), agent_id)
    }

    /// The same, naming the file directly.
    ///
    /// **`agent_seq` is seeded from the file's own tail, not from 0.** A resume opens this file
    /// again, and a writer that restarted the sequence would put two events with one ordinal in one
    /// file — §7.3.3's seam is stated in ordinals, so that is not a cosmetic duplicate but an
    /// ambiguous read point. Reading the file on open costs one pass over a file this process is
    /// about to append to anyway.
    ///
    /// A file that cannot be *parsed* still yields a next ordinal: the fold stops at the intact
    /// prefix and the writer continues past it. Refusing to open would mean a node could not record
    /// what it is doing now because of a line written before it started.
    pub fn open_path(path: &Path, agent_id: &AgentId) -> Result<Self, EventError> {
        if let Some(dir) = path.parent() {
            crate::private_fs::create_dir_all(dir)?;
        }
        // The fresh file after a rotation may hold nothing yet; the ordinals then run on from
        // the rotated one.
        let next_in = |p: &Path| -> Result<Option<u64>, EventError> {
            match std::fs::read(p) {
                Ok(bytes) => Ok(marion_core::event::read(&bytes).0.next_seq()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        };
        let seq = match next_in(path)? {
            Some(seq) => seq,
            None => next_in(&rotated_path(path))?.unwrap_or(0),
        };
        let file = crate::private_fs::open_append(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            len: file.metadata()?.len(),
            cap: MAX_EVENTS_FILE_BYTES,
            file,
            agent_id: agent_id.clone(),
            seq,
            dirty: false,
            last_sync: Instant::now(),
            start: Instant::now(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This writer with a cap other than [`MAX_EVENTS_FILE_BYTES`], so a test can reach it.
    #[cfg(test)]
    fn capped_at(mut self, cap: u64) -> Self {
        self.cap = cap;
        self
    }

    /// The ordinal the next appended event will carry.
    pub fn next_seq(&self) -> u64 {
        self.seq
    }

    /// **§4.2's `mono_ns` origin, lent out.**
    ///
    /// `pty.cast` records its intervals against this instant rather than one of its own. §4.2 gives
    /// `mono_ns` the job of *aligning* a node's two streams, and alignment against two different
    /// zeroes is not alignment: a cast that called `Instant::now()` in its own constructor would be
    /// offset from `events.jsonl` by however long the node's setup took, with nothing on either
    /// file recording the offset. `mono_ns_and_the_cast_share_one_epoch` is the guard.
    pub fn origin(&self) -> Instant {
        self.start
    }

    /// Append one event, applying §4.3's commit policy.
    ///
    /// The payload is passed through [`bound`] first, so an over-large frame is recorded shortened
    /// rather than refused — `event.rs` argues why that is the honest answer here and the wrong one
    /// for the journal. The returned [`Event`] is **what was written**, shortening included, so a
    /// caller can see what the file says rather than what it offered.
    pub fn append(&mut self, draft: Draft) -> Result<Event, EventError> {
        let event = bound(Event {
            agent_id: self.agent_id.clone(),
            agent_seq: self.seq,
            src_seq: draft.src_seq,
            caused_by: draft.caused_by,
            ts: SystemTime(std::time::SystemTime::now()),
            mono_ns: self.start.elapsed().as_nanos() as u64,
            provenance: draft.provenance,
            payload: draft.payload,
        });
        let barrier = event.is_barrier();
        let line = encode(&event)?;
        if self.len > 0 && self.len + line.len() as u64 > self.cap {
            self.rotate()?;
        }
        self.file.write_all(&line)?;
        self.len += line.len() as u64;
        self.seq += 1;
        self.dirty = true;
        if barrier || self.last_sync.elapsed() >= GROUP_COMMIT_INTERVAL {
            self.sync()?;
        }
        Ok(event)
    }

    /// **Close the full file as `events.jsonl.1` and continue in a fresh one**, replacing an older
    /// rotated half. Synced first, so the rotated half is whole on disk; a reader following the
    /// old file finishes it from the new name ([`EventReader::poll`]).
    fn rotate(&mut self) -> Result<(), EventError> {
        self.sync()?;
        std::fs::rename(&self.path, rotated_path(&self.path))?;
        self.file = crate::private_fs::open_append(&self.path)?;
        self.len = 0;
        Ok(())
    }

    /// Best-effort append for a call site that must not change its behaviour on an I/O fault.
    ///
    /// Same policy as `journal::Journal::record`, and for the same reason stated there: a run that
    /// succeeds today must still succeed, and a viewer's file filling up must not kill a real child
    /// mid-edit. Loud on stderr rather than swallowed — a silent failure is the shape §12 keeps
    /// recording. Unlike the journal there is no path here that ever becomes fatal: nothing decides
    /// whether a process may be spawned by reading this file.
    pub fn record(&mut self, draft: Draft) {
        if let Err(e) = self.append(draft) {
            eprintln!("marion: event write failed: {e}");
        }
    }

    /// fsync if anything is pending. `sync_data` rather than `sync_all`: the file's length and
    /// contents are the durability claim, its mtime is not.
    pub fn sync(&mut self) -> Result<(), EventError> {
        if self.dirty {
            self.file.sync_data()?;
            self.dirty = false;
        }
        self.last_sync = Instant::now();
        Ok(())
    }

    /// §4.3's timer half, for a caller with an idle loop between frames.
    pub fn tick(&mut self) {
        if self.dirty && self.last_sync.elapsed() >= GROUP_COMMIT_INTERVAL {
            let _ = self.sync();
        }
    }
}

impl Drop for EventWriter {
    /// The last group's commit. Without it a clean shutdown could lose trailing frames that a crash
    /// would have lost anyway — a difference a reader could not explain.
    fn drop(&mut self) {
        let _ = self.sync();
    }
}

/// A node's stream, **replayed and then followed by one cursor** — §7.3.3's re-attach, both legs.
///
/// [`Self::open_path`] is the replay leg: it reads the file to its intact prefix and hands back
/// every event in it. [`Self::poll`] is the subscribe leg, continuing from **the same offset**. The
/// module doc argues why that is the whole answer to the seam: there is no splice to get wrong,
/// because there is nothing being spliced.
///
/// Poll-driven and clock-free, like [`crate::registry::Registry`], so it is testable with no thread
/// and no timer. Never fails and never panics: every way this can go wrong is a
/// [`crate::registry::Status`].
///
/// How much of a file's start [`EventReader`] keeps to tell it from another file at the same inode
/// number: the first frame's envelope, its ordinal and its timestamp.
const HEAD_BYTES: usize = 128;

pub struct EventReader {
    path: PathBuf,
    log: EventLog,
    /// Bytes folded in. Always a record boundary — the intact prefix and no further.
    offset: u64,
    /// The inode `offset` is into. A different one at the path is a rotation
    /// ([`EventWriter::rotate`]), and the old file is finished from its rotated name.
    ino: Option<u64>,
    /// The first bytes of the file `offset` is into, as this reader read them ([`HEAD_BYTES`] at
    /// most). An inode number can be reused once two rotations free it, so a file whose first
    /// bytes differ is another file even at the same number.
    head: Vec<u8>,
    status: crate::registry::Status,
    polls: u64,
    /// Whether the file has ever been opened. See [`Self::ever_written`].
    seen_file: bool,
}

impl EventReader {
    /// Attach to a node's stream inside its agent dir.
    pub fn open(agent: &AgentDir) -> Result<(Self, Vec<Event>), EventError> {
        Self::open_path(&agent.events())
    }

    /// The same, naming the file directly. **This is the replay leg**, and its `Vec<Event>` is what
    /// §7.3.3 means by *"replay to the journal's own read point"* — the point being
    /// [`Self::read_point`], taken from the same reading.
    ///
    /// **A missing file is not an error**, and not an empty stream either: see
    /// [`Self::ever_written`]. An unreadable one is [`crate::registry::Status::Unreadable`] rather
    /// than a failure, because a client attaching to a tree must not be refused the whole tree by
    /// one node's permissions fault.
    pub fn open_path(path: &Path) -> Result<(Self, Vec<Event>), EventError> {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        // The current file is opened first and the rotated half read only if it is another file:
        // a rotation between the two opens would otherwise replay one file twice.
        let current = match File::open(path) {
            Ok(f) => Some(f),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let ino = match &current {
            Some(f) => Some(f.metadata()?.ino()),
            None => None,
        };
        let older = match File::open(rotated_path(path)) {
            Ok(mut f) if Some(f.metadata()?.ino()) != ino => {
                let mut bytes = Vec::new();
                f.read_to_end(&mut bytes)?;
                Some(bytes)
            }
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let seen_file = current.is_some() || older.is_some();
        let mut bytes = Vec::new();
        if let Some(mut f) = current {
            f.read_to_end(&mut bytes)?;
        }
        let mut log = EventLog::default();
        let mut events = Vec::new();
        if let Some(older) = older {
            log.extend(&older, &mut events);
        }
        let consumed = log.extend(&bytes, &mut events);
        let status = status_for(&log, path, consumed as u64);
        Ok((
            Self {
                path: path.to_path_buf(),
                offset: consumed as u64,
                ino,
                head: bytes[..bytes.len().min(HEAD_BYTES)].to_vec(),
                log,
                status,
                polls: 0,
                seen_file,
            },
            events,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The reading's own account of itself — counts, ordering evidence, and what it could not read.
    pub fn log(&self) -> &EventLog {
        &self.log
    }

    /// §4.2's loss, per node. Reported, never repaired — see `marion_core::event::EventGap`.
    pub fn gaps(&self) -> &[marion_core::event::EventGap] {
        &self.log.gaps
    }

    pub fn status(&self) -> &crate::registry::Status {
        &self.status
    }

    /// Polls attempted. With [`Self::read_point`], what separates *"this node has produced nothing
    /// new"* from *"nobody is reading this node"* — two states with an identical stream.
    pub fn polls(&self) -> u64 {
        self.polls
    }

    /// **Whether anything ever recorded this node at all.**
    ///
    /// The negative control this reader exists to make possible. A node with no `events.jsonl` and
    /// a node whose `events.jsonl` holds nothing produce the identical empty replay and the
    /// identical [`Self::read_point`] — and they are different facts: the first is a node marion
    /// never observed, the second is a node marion observed being silent. Collapsing them would
    /// report an unrecorded node as an empty transcript, which is this repository's documented
    /// failure class (a reader claiming completeness it never had). Same split
    /// `Registry::seen_file` makes for the journal.
    pub fn ever_written(&self) -> bool {
        self.seen_file
    }

    /// **The seam value**, in the vocabulary §7.3.3's re-attach already speaks
    /// (`marion_core::proto::AttachMode` carries one on every arm).
    ///
    /// `records` is what this cursor has delivered; `src_seq` is the last source-side ordering
    /// evidence any event carried, `None` where the harness supplies none — which on Codex and on
    /// Claude Code `headless` is most of what marion runs, and where §4.2 forbids implying loss
    /// detection marion does not have.
    pub fn read_point(&self) -> marion_core::proto::ReplayPoint {
        marion_core::proto::ReplayPoint {
            records: self.log.records,
            src_seq: self.log.last_src_seq.clone(),
        }
    }

    /// **The subscribe leg.** Fold in whatever has been appended since the last read, appending
    /// every new event to `out`, and answer how many that was.
    ///
    /// Zero is the common answer and costs one `open` and one `stat`. It never re-delivers an event
    /// `open_path` already returned, because it continues from that read's own offset — which is
    /// the entire correctness argument, and the thing
    /// `a_replay_then_subscribe_loses_nothing_and_repeats_nothing_across_a_torn_seam` holds.
    pub fn poll(&mut self, out: &mut Vec<Event>) -> usize {
        self.polls += 1;
        if matches!(self.status, crate::registry::Status::Stopped { .. }) {
            return 0;
        }
        let before = self.log.records;
        if let Some(tail) = self.rotated_tail() {
            self.log.extend(&tail, out);
            self.offset = 0;
        }
        match self.read_new() {
            Ok(None) => {
                // A read that succeeded and found nothing **clears** a previous `Unreadable`: that
                // status is a claim the stream may be stale, and leaving it set after marion has
                // looked and found the file intact would keep asserting a staleness that is over.
                self.status = crate::registry::Status::Following;
            }
            Ok(Some(fresh)) => {
                self.offset += self.log.extend(&fresh, out) as u64;
                self.status = status_for(&self.log, &self.path, self.offset);
            }
            Err(status) => self.status = status,
        }
        (self.log.records - before) as usize
    }

    /// **The rest of a file the writer rotated away**, when the path now names another one: the
    /// bytes past the cursor, read from the rotated name while it is still the same file. `Some`
    /// moves the cursor to the new file; an empty `Some` is a file rotated away twice since the
    /// last poll, whose loss the ordinals then show. `None` is no rotation.
    fn rotated_tail(&mut self) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let was = self.ino?;
        let meta = std::fs::metadata(&self.path).ok()?;
        let now = meta.ino();
        // Two rotations can free the old inode and hand its number to the fresh file (ext4 does),
        // and the number alone would then hide the rotation — and every frame after it. The file
        // only grows and its start never changes, so one shorter than the cursor, or one that
        // begins with other bytes, is another file at the same number.
        if now == was && meta.len() >= self.offset && self.same_head() {
            return None;
        }
        self.ino = Some(now);
        self.head.clear();
        let mut tail = Vec::new();
        if let Ok(mut f) = File::open(rotated_path(&self.path))
            && f.metadata().is_ok_and(|m| m.ino() == was)
            && f.seek(SeekFrom::Start(self.offset)).is_ok()
        {
            let _ = f.read_to_end(&mut tail);
        }
        Some(tail)
    }

    /// Whether the file at the path still begins with [`Self::head`].
    fn same_head(&self) -> bool {
        use std::io::Read;
        if self.head.is_empty() {
            return true;
        }
        let mut start = vec![0u8; self.head.len()];
        File::open(&self.path)
            .and_then(|mut f| f.read_exact(&mut start))
            .is_ok_and(|()| start == self.head)
    }

    /// The bytes past the cursor. `Ok(None)` is "nothing new".
    fn read_new(&mut self) -> Result<Option<Vec<u8>>, crate::registry::Status> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            // A file that does not exist *yet* is a node that has not spoken, not a fault — the
            // writer creates it on its first append. One that stops existing after this reader saw
            // it is the opposite, and the two are the same `NotFound` from the syscall.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.seen_file => {
                return Ok(None);
            }
            Err(e) => {
                return Err(crate::registry::Status::Unreadable {
                    reason: format!("opening {}: {e}", self.path.display()),
                });
            }
        };
        self.seen_file = true;
        let meta = f
            .metadata()
            .map_err(|e| crate::registry::Status::Unreadable {
                reason: format!("stat {}: {e}", self.path.display()),
            })?;
        let len = meta.len();
        if self.ino.is_none() {
            use std::os::unix::fs::MetadataExt;
            self.ino = Some(meta.ino());
        }
        if len < self.offset {
            // Append-only means bytes already read never change. A shorter file is a different
            // file, and continuing would narrate one stream's bytes as another's.
            return Err(crate::registry::Status::Stopped {
                reason: format!(
                    "{} shrank from {} to {len} bytes; an append-only stream cannot get shorter, \
                     so this reader is following a file that is no longer the one it replayed",
                    self.path.display(),
                    self.offset
                ),
            });
        }
        if len == self.offset {
            return Ok(None);
        }
        f.seek(SeekFrom::Start(self.offset))
            .map_err(|e| crate::registry::Status::Unreadable {
                reason: format!("seeking {}: {e}", self.path.display()),
            })?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        f.read_to_end(&mut buf)
            .map_err(|e| crate::registry::Status::Unreadable {
                reason: format!("reading {}: {e}", self.path.display()),
            })?;
        if self.offset == 0 && self.head.is_empty() {
            self.head = buf[..buf.len().min(HEAD_BYTES)].to_vec();
        }
        Ok(Some(buf))
    }
}

/// One mapping from a reading to a [`crate::registry::Status`], shared by the replay and subscribe
/// legs so the two can never disagree about what the file is doing.
///
/// [`crate::registry::Status`] is **reused rather than twinned**: its three-values-not-two argument
/// ("the collapses are the failure") is the same argument here, and a second enum with the same
/// shape and the same doc is exactly the duplication that lets one copy drift into meaning
/// something else.
fn status_for(log: &EventLog, path: &Path, offset: u64) -> crate::registry::Status {
    match &log.truncation {
        Some(Truncation::Unparsable { line, .. }) => crate::registry::Status::Stopped {
            reason: format!(
                "{} line {line} (at byte {offset}) is a complete line that is not an event. An \
                 append-only stream cannot heal a bad line, so this reader stops rather than \
                 narrating a stream from bytes marion cannot read.",
                path.display()
            ),
        },
        // A torn final line is what a file another process is appending to looks like (§7.4), and
        // what a crash leaves. Nothing to report; it is re-read from the same offset.
        Some(Truncation::UnterminatedTail { .. }) | None => crate::registry::Status::Following,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::{AgentId, ExitStatus, ProcessExit};
    use marion_core::event::{EventLog, Lifecycle, MAX_EVENT_BYTES, Payload, PayloadKind};
    use marion_core::harness::Harness;
    use marion_core::ir::{Completeness, Provenance, Source};
    use marion_testsupport::scratch;
    use serde_json::json;
    use std::time::Duration;

    fn node() -> AgentId {
        AgentId("a-1".into())
    }

    /// **A node's event stream carries its transcript, so its directory is `0700` and the file
    /// `0600`** — created that way, not left to the umask.
    ///
    /// Mutation: open with plain `create_dir_all`/`OpenOptions` and both modes read `0755`/`0644`.
    #[test]
    fn a_fresh_agent_directory_and_event_stream_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("events-private");
        let project = marion_core::paths::ProjectDir::new(&dir, Path::new("/repo"));
        let agent = project.agent(&node());
        let mut w = EventWriter::open(&agent, &node()).unwrap();
        w.append(frame("k")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(project.path()), 0o700);
        assert_eq!(mode(agent.path()), 0o700);
        assert_eq!(mode(&agent.events()), 0o600);
    }

    fn frame(key: &str) -> Draft {
        Draft::observed(
            Payload::Vendor {
                harness: Harness::Codex.into(),
                key: key.into(),
                json: json!({"item": {"id": "msg_09cb"}}),
            },
            Source::Protocol,
        )
    }

    fn read(path: &std::path::Path) -> (EventLog, Vec<marion_core::event::Event>) {
        marion_core::event::read(&std::fs::read(path).unwrap_or_default())
    }

    #[test]
    fn a_written_stream_reads_back_as_the_events_that_were_written() {
        let dir = scratch("events-round-trip");
        let path = dir.join("events.jsonl");
        {
            let mut w = EventWriter::open_path(&path, &node()).unwrap();
            w.append(Draft::marion(Payload::Lifecycle(Lifecycle::Opened)))
                .unwrap();
            w.append(frame("item.started")).unwrap();
            w.append(Draft::observed(
                Payload::Raw("Warning: Basic terminal detected".into()),
                Source::Protocol,
            ))
            .unwrap();
            w.append(Draft::marion(Payload::Lifecycle(Lifecycle::Exited {
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "clean exit".into(),
                },
            })))
            .unwrap();
        }
        let (log, events) = read(&path);
        assert_eq!(log.records, 4);
        assert_eq!(log.truncation, None);
        assert!(log.gaps.is_empty(), "one writer, gapless by construction");
        assert_eq!(
            events.iter().map(|e| e.agent_seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(events.iter().all(|e| e.agent_id == node()));
        assert_eq!(
            events[1].provenance.source,
            Source::Protocol,
            "a harness frame is not marion's own observation"
        );
        assert_eq!(events[0].provenance.source, Source::Marion);
    }

    fn seqs(events: &[marion_core::event::Event]) -> Vec<u64> {
        events.iter().map(|e| e.agent_seq).collect()
    }

    /// **A stream past its cap rotates once, into `events.jsonl.1`**: the node's files stay at
    /// most two caps, the ordinals run on across the two, and a resume after a rotation continues
    /// them rather than restarting at 0 on the fresh file.
    #[test]
    fn a_stream_past_its_cap_rotates_and_its_ordinals_run_on() {
        let dir = scratch("events-rotate");
        let path = dir.join("events.jsonl");
        let rotated = rotated_path(&path);
        let cap = 2_000;
        let mut w = EventWriter::open_path(&path, &node())
            .unwrap()
            .capped_at(cap);
        for i in 0..60 {
            w.append(frame(&format!("frame-{i}"))).unwrap();
        }
        drop(w);
        let len = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        assert!(len(&rotated) > 0, "the stream rotated");
        assert!(
            len(&path) <= cap && len(&rotated) <= cap,
            "neither file passes the cap"
        );
        let (_, older) = read(&rotated);
        let (_, newer) = read(&path);
        let last = newer.last().unwrap().agent_seq;
        assert_eq!(last, 59, "every ordinal is accounted for up to the last");
        assert_eq!(
            seqs(&older).last().unwrap() + 1,
            newer[0].agent_seq,
            "the fresh file continues the rotated one"
        );

        std::fs::write(&path, b"").unwrap();
        let resumed = EventWriter::open_path(&path, &node()).unwrap();
        assert_eq!(
            resumed.next_seq(),
            seqs(&older).last().unwrap() + 1,
            "an empty fresh file seeds from the rotated one"
        );
    }

    /// **A reader follows a rotation without losing or repeating an event**, and a reader that
    /// opens after one replays both files.
    #[test]
    fn a_reader_follows_a_rotation_and_a_late_reader_replays_both_files() {
        let dir = scratch("events-rotate-read");
        let path = dir.join("events.jsonl");
        let mut w = EventWriter::open_path(&path, &node())
            .unwrap()
            .capped_at(2_000);
        w.append(frame("first")).unwrap();
        let (mut reader, replayed) = EventReader::open_path(&path).unwrap();
        let mut seen = seqs(&replayed);
        for i in 0..60 {
            w.append(frame(&format!("frame-{i}"))).unwrap();
            if i % 3 == 0 {
                let mut out = Vec::new();
                reader.poll(&mut out);
                seen.extend(seqs(&out));
            }
        }
        let mut out = Vec::new();
        reader.poll(&mut out);
        seen.extend(seqs(&out));
        assert_eq!(
            seen,
            (0..=60).collect::<Vec<u64>>(),
            "{:?}",
            reader.status()
        );
        assert!(matches!(
            reader.status(),
            crate::registry::Status::Following
        ));

        let (_, late) = EventReader::open_path(&path).unwrap();
        let late = seqs(&late);
        assert_eq!(late.last(), Some(&60));
        assert!(
            late.windows(2).all(|w| w[1] == w[0] + 1),
            "the rotated file then the fresh one, in order: {late:?}"
        );
    }

    /// **Another file at the same inode number is still another file.** Two rotations can hand the
    /// old number to the fresh file (ext4 does); rewriting the path in place is that case on any
    /// filesystem: a file with other first bytes, longer than the cursor, at the number the reader
    /// knows. It is read as the rotation it is, not skipped as the file the reader was in.
    #[test]
    fn a_fresh_file_that_reuses_the_readers_inode_number_is_still_read() {
        let dir = scratch("events-reused-inode");
        let path = dir.join("events.jsonl");
        let mut w = EventWriter::open_path(&path, &node()).unwrap();
        w.append(frame("first")).unwrap();
        let (mut reader, _) = EventReader::open_path(&path).unwrap();
        let other = dir.join("fresh.jsonl");
        let mut fresh = EventWriter::open_path(&other, &node()).unwrap();
        for i in 0..6 {
            fresh.append(frame(&format!("fresh-{i}"))).unwrap();
        }
        // A rotated file continues the node's ordinals, so its first frame is never the old
        // file's: drop the fresh file's ordinal 0, which could match the old head byte for byte
        // within one millisecond.
        let bytes = std::fs::read(&other).unwrap();
        let from = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
        let before = std::fs::metadata(&path).unwrap();
        // In place, so the inode number is the one the reader holds.
        std::fs::write(&path, &bytes[from..]).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), before.ino());
        let mut out = Vec::new();
        reader.poll(&mut out);
        assert_eq!(seqs(&out).len(), 5, "{:?}", reader.status());
    }

    /// **A file rotated away twice between two polls is a reported gap**, never a silent one: the
    /// reader moves on to the current file and the ordinals it skipped are named.
    #[test]
    fn a_reader_that_falls_two_rotations_behind_reports_the_gap() {
        let dir = scratch("events-rotate-gap");
        let path = dir.join("events.jsonl");
        let mut w = EventWriter::open_path(&path, &node())
            .unwrap()
            .capped_at(1_000);
        w.append(frame("first")).unwrap();
        let (mut reader, _) = EventReader::open_path(&path).unwrap();
        for i in 0..60 {
            w.append(frame(&format!("frame-{i}"))).unwrap();
        }
        let mut out = Vec::new();
        reader.poll(&mut out);
        assert_eq!(out.last().map(|e| e.agent_seq), Some(60));
        assert!(!reader.gaps().is_empty(), "the skipped ordinals are named");
        assert!(matches!(
            reader.status(),
            crate::registry::Status::Following
        ));
    }

    /// A resume opens the file again. Restarting at 0 would put two events with one ordinal in one
    /// file, and §7.3.3's seam is stated in ordinals.
    #[test]
    fn agent_seq_continues_from_the_file_rather_than_restarting_at_zero() {
        let dir = scratch("events-resume");
        let path = dir.join("events.jsonl");
        EventWriter::open_path(&path, &node())
            .unwrap()
            .append(frame("one"))
            .unwrap();
        let mut second = EventWriter::open_path(&path, &node()).unwrap();
        assert_eq!(second.next_seq(), 1, "seeded from the file's own tail");
        second.append(frame("two")).unwrap();

        let (log, events) = read(&path);
        assert_eq!(log.records, 2, "the second open must not have truncated");
        assert_eq!(
            events.iter().map(|e| e.agent_seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(log.gaps.is_empty());
    }

    /// **NC — the one-writer invariant is checked, not assumed.**
    ///
    /// Everything in this module rests on one process owning one node's file. That is true by
    /// construction today, and a property true only by construction is true until somebody changes
    /// the construction. So: two writers, opened against one file, must not produce a stream that
    /// reads as plausible. They produce duplicate ordinals, and the reading says so.
    #[test]
    fn a_second_writer_on_one_file_is_visible_as_duplicate_ordinals_not_a_plausible_stream() {
        let dir = scratch("events-two-writers");
        let path = dir.join("events.jsonl");
        let mut a = EventWriter::open_path(&path, &node()).unwrap();
        let mut b = EventWriter::open_path(&path, &node()).unwrap();
        a.append(frame("a-0")).unwrap();
        b.append(frame("b-0")).unwrap();
        a.append(frame("a-1")).unwrap();

        let (log, events) = read(&path);
        assert_eq!(log.records, 3, "every line is still a readable event");
        assert_eq!(
            events.iter().map(|e| e.agent_seq).collect::<Vec<_>>(),
            vec![0, 0, 1],
            "both writers believed they owned the sequence"
        );
        // **Once, at the repeat — and then the sequence re-converges**, because writer `a`'s next
        // ordinal happens to be the one the reading was expecting again. That is precisely why a
        // gap is *reported* and never repaired: after this point the file reads as a clean
        // sequence, and the only surviving evidence that two writers were here is this record.
        assert_eq!(
            log.gaps.len(),
            1,
            "and the reading refuses to present that as one stream: {:?}",
            log.gaps
        );
        // The direction is what separates a duplicate from a loss, and both are reportable.
        assert!(
            log.gaps[0].found < log.gaps[0].expected,
            "a repeated ordinal, not a missing one: {:?}",
            log.gaps[0]
        );
    }

    /// The counterpart: the same reading distinguishes a *lost* event from a repeated one, so the
    /// test above is not passing on a check that fires either way.
    #[test]
    fn a_lost_event_reads_as_a_gap_forward_where_a_duplicate_reads_as_a_gap_back() {
        let dir = scratch("events-lost");
        let path = dir.join("events.jsonl");
        let mut w = EventWriter::open_path(&path, &node()).unwrap();
        w.append(frame("zero")).unwrap();
        w.append(frame("one")).unwrap();
        w.append(frame("two")).unwrap();
        drop(w);
        // Excise the middle line, as a lost record would.
        let text = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = text
            .lines()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, l)| l)
            .collect();
        std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();

        let (log, events) = read(&path);
        assert_eq!(events.len(), 2, "what survived is still served");
        assert_eq!(log.gaps.len(), 1);
        assert!(
            log.gaps[0].found > log.gaps[0].expected,
            "forward: an event was written and is not here — {:?}",
            log.gaps[0]
        );
    }

    #[test]
    fn a_lifecycle_bookend_is_durable_before_append_returns_and_a_frame_rides_the_timer() {
        let dir = scratch("events-barrier");
        let path = dir.join("events.jsonl");
        let mut w = EventWriter::open_path(&path, &node()).unwrap();
        w.append(Draft::marion(Payload::Lifecycle(Lifecycle::Opened)))
            .unwrap();
        assert!(!w.dirty, "a bookend fsyncs before append returns");

        w.append(frame("item.started")).unwrap();
        assert!(
            w.dirty,
            "a content frame rides the ~50 ms timer — §4.3 buys an fsync only for what a crash \
             must not lose"
        );
        // …and is readable immediately either way: the bytes are in the file.
        assert_eq!(read(&path).0.records, 2);
    }

    #[test]
    fn the_group_commit_timer_flushes_a_pending_event() {
        let dir = scratch("events-timer");
        let path = dir.join("events.jsonl");
        let mut w = EventWriter::open_path(&path, &node()).unwrap();
        w.append(frame("item.started")).unwrap();
        assert!(w.dirty);
        std::thread::sleep(crate::journal::GROUP_COMMIT_INTERVAL + Duration::from_millis(10));
        w.tick();
        assert!(!w.dirty);
    }

    /// The writer applies `bound`, so an over-large frame costs its body and never its place.
    #[test]
    fn an_oversized_frame_is_written_shortened_rather_than_refused_or_dropped() {
        let dir = scratch("events-oversized");
        let path = dir.join("events.jsonl");
        let mut w = EventWriter::open_path(&path, &node()).unwrap();
        w.append(frame("small")).unwrap();
        w.append(Draft::observed(
            Payload::Raw("x".repeat(MAX_EVENT_BYTES + 1_000)),
            Source::Protocol,
        ))
        .unwrap();
        w.append(frame("after")).unwrap();
        drop(w);

        let (log, events) = read(&path);
        assert_eq!(log.records, 3, "the over-large event is still an event");
        assert!(log.gaps.is_empty(), "and still holds its ordinal");
        match &events[1].payload {
            Payload::Oversized { was, bytes } => {
                assert_eq!(*was, PayloadKind::Raw);
                assert!(*bytes > MAX_EVENT_BYTES);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(events[1].provenance.completeness, Completeness::Partial);
        assert_eq!(
            events[2].provenance.completeness,
            Completeness::Complete,
            "shortening one event says nothing about the next"
        );
    }

    /// The adapter from what the driver already observes to what this file records. Both variants,
    /// because `duplex::StreamEvent` has two for a measured reason and a writer that only handled
    /// `Frame` would render a node that printed a stack trace as an unexplained silence.
    #[test]
    fn a_stream_event_becomes_the_payload_the_node_actually_produced() {
        let v = json!({"type": "assistant", "uuid": "u-1"});
        match from_stream_event(Harness::ClaudeCode, StreamEvent::Frame(&v)).payload {
            Payload::Vendor { harness, key, json } => {
                assert_eq!(harness, Harness::ClaudeCode);
                assert_eq!(
                    key, "assistant",
                    "the harness's own discriminator, verbatim"
                );
                assert_eq!(json, v);
            }
            other => panic!("{other:?}"),
        }
        match from_stream_event(
            Harness::OpenCode,
            StreamEvent::Unparsed("[STARTUP] Phase 1"),
        )
        .payload
        {
            Payload::Raw(s) => assert_eq!(s, "[STARTUP] Phase 1"),
            other => panic!("{other:?}"),
        }
        // A frame with no recognisable discriminator is still carried; `key` is empty rather than
        // invented, which is the same refusal `handler/summary.rs` makes about a fact it cannot
        // source.
        match from_stream_event(Harness::Codex, StreamEvent::Frame(&json!({"a": 1}))).payload {
            Payload::Vendor { key, .. } => assert_eq!(key, ""),
            other => panic!("{other:?}"),
        }
    }

    // --- the reader, and §7.3.3's seam ------------------------------------------------------

    fn writer(path: &std::path::Path) -> EventWriter {
        EventWriter::open_path(path, &node()).unwrap()
    }

    /// **NC — "nobody recorded this node" must never read as "this node said nothing".**
    ///
    /// Both states are reachable through the real API and they are different facts about the run:
    /// one is a node marion never observed, the other is a node marion observed being silent. A
    /// reader that collapsed them would report an unrecorded node as an empty transcript.
    #[test]
    fn a_node_whose_events_were_never_written_is_distinguishable_from_one_that_produced_none() {
        let dir = scratch("events-never-written");
        let absent = dir.join("never/events.jsonl");
        let silent = dir.join("silent/events.jsonl");

        let (unwritten, replay) = EventReader::open_path(&absent).unwrap();
        assert!(replay.is_empty());
        assert!(
            !unwritten.ever_written(),
            "no file: nothing ever recorded this node"
        );

        // A writer that opened the file and appended nothing — a node that produced no output.
        drop(writer(&silent));
        let (empty, replay) = EventReader::open_path(&silent).unwrap();
        assert!(replay.is_empty(), "same zero events…");
        assert!(
            empty.ever_written(),
            "…and a different fact: marion was recording, the node said nothing"
        );
        assert_eq!(unwritten.read_point(), empty.read_point());
        assert_ne!(
            unwritten.ever_written(),
            empty.ever_written(),
            "the read point cannot separate them, which is exactly why this flag exists"
        );
    }

    /// A node that ran entirely inside the detached window — §7.3.3's *"spawned, ran and terminated
    /// entirely within"* case. Replay alone must show a client that it started **and** that it
    /// finished, which is what the bookends are for.
    #[test]
    fn a_node_that_began_and_ended_unobserved_replays_as_both_from_the_file_alone() {
        let dir = scratch("events-whole-life");
        let path = dir.join("events.jsonl");
        {
            let mut w = writer(&path);
            w.append(Draft::marion(Payload::Lifecycle(Lifecycle::Opened)))
                .unwrap();
            w.append(frame("item.completed")).unwrap();
            w.append(Draft::marion(Payload::Lifecycle(Lifecycle::Exited {
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "clean exit".into(),
                },
            })))
            .unwrap();
        }
        let (r, replay) = EventReader::open_path(&path).unwrap();
        assert_eq!(replay.len(), 3);
        assert!(matches!(
            replay[0].payload,
            Payload::Lifecycle(Lifecycle::Opened)
        ));
        assert!(
            matches!(
                &replay[2].payload,
                Payload::Lifecycle(Lifecycle::Exited { status, .. }) if *status == ExitStatus::Ok
            ),
            "without a terminal bookend this is indistinguishable from a stream cut mid-turn"
        );
        assert_eq!(r.read_point().records, 3);
    }

    /// **NC — the seam has neither a gap nor a duplicate, with the writer appending across it.**
    ///
    /// M2 criterion 4(iii). The hard shape is not "append after the replay finished" but a replay
    /// that lands **mid-record**: the writer is another process, so the replay leg can legitimately
    /// stop on a half-written line. If the cursor advanced past it the event is lost; if it
    /// restarted before it the event is delivered twice. One cursor makes both unreachable, and
    /// this is the test that would catch either.
    #[test]
    fn a_replay_then_subscribe_loses_nothing_and_repeats_nothing_across_a_torn_seam() {
        let dir = scratch("events-seam");
        let path = dir.join("events.jsonl");
        let mut w = writer(&path);
        w.append(frame("zero")).unwrap();
        w.append(frame("one")).unwrap();
        w.sync().unwrap();

        // A third event, **half written** at the instant the client attaches.
        let third = marion_core::event::encode(&marion_core::event::Event {
            agent_id: node(),
            agent_seq: 2,
            src_seq: None,
            caused_by: None,
            ts: marion_core::encoding::SystemTime(std::time::SystemTime::now()),
            mono_ns: 0,
            provenance: Provenance::marion(),
            payload: Payload::Raw("the tail that was not finished".into()),
        })
        .unwrap();
        let (head, rest) = third.split_at(third.len() / 2);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(head)
            .unwrap();

        // The replay leg.
        let (mut reader, mut seen) = EventReader::open_path(&path).unwrap();
        assert_eq!(seen.len(), 2, "the torn third event is not replayed");
        assert!(matches!(
            reader.log().truncation,
            Some(marion_core::registry::Truncation::UnterminatedTail { .. })
        ));
        let seam = reader.read_point();
        assert_eq!(seam.records, 2);

        // The rest of the third event lands, and a fourth after it.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(rest).unwrap();
        }
        let mut w2 = EventWriter::open_path(&path, &node()).unwrap();
        assert_eq!(
            w2.next_seq(),
            3,
            "the writer resumes past the completed line"
        );
        w2.append(frame("three")).unwrap();
        w2.sync().unwrap();

        // The subscribe leg, from the same cursor.
        let n = reader.poll(&mut seen);
        assert_eq!(n, 2, "the completed third and the fourth, once each");
        assert_eq!(
            seen.iter().map(|e| e.agent_seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "contiguous across the seam: no hole, no repeat"
        );
        assert!(
            reader.gaps().is_empty(),
            "and the reading agrees: {:?}",
            reader.gaps()
        );
        assert_eq!(reader.read_point().records, 4);
        assert!(seam.records < reader.read_point().records);
    }

    /// The control for the control: if the cursor were reset instead of carried, the seam test
    /// above would still see every event — in the right order — and only a duplicate would betray
    /// it. So assert the *count*, which a re-read inflates.
    #[test]
    fn a_reader_that_re_read_from_the_start_would_be_caught_by_the_count_not_the_order() {
        let dir = scratch("events-seam-control");
        let path = dir.join("events.jsonl");
        let mut w = writer(&path);
        w.append(frame("zero")).unwrap();
        w.sync().unwrap();
        let (mut reader, mut seen) = EventReader::open_path(&path).unwrap();
        w.append(frame("one")).unwrap();
        w.sync().unwrap();
        reader.poll(&mut seen);
        assert_eq!(seen.len(), 2);
        // A hand-rolled re-read is what a two-cursor implementation would produce.
        let (_, whole) = EventReader::open_path(&path).unwrap();
        assert_eq!(whole.len(), 2);
        assert_ne!(
            seen.len(),
            whole.len() + 1,
            "the seam must not have re-delivered event zero"
        );
        assert_eq!(
            seen.iter().map(|e| e.agent_seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn a_quiet_stream_is_not_a_stale_one() {
        let dir = scratch("events-quiet");
        let path = dir.join("events.jsonl");
        let mut w = writer(&path);
        w.append(frame("zero")).unwrap();
        w.sync().unwrap();
        let (mut reader, mut seen) = EventReader::open_path(&path).unwrap();
        let before = reader.read_point();
        assert_eq!(reader.poll(&mut seen), 0);
        assert_eq!(reader.read_point(), before, "quiet is not stale");
        assert_eq!(reader.polls(), 1, "…and somebody is looking");
        assert!(matches!(
            reader.status(),
            crate::registry::Status::Following
        ));
    }

    /// `watch.rs`'s one refusal, kept: a complete line that is not an event stops the reader for
    /// good rather than being skipped. An append-only file cannot heal a bad line, and narrating
    /// past bytes marion does not understand would be inventing a stream.
    #[test]
    fn a_line_that_is_not_an_event_stops_the_reader_rather_than_being_skipped() {
        let dir = scratch("events-corrupt");
        let path = dir.join("events.jsonl");
        let mut w = writer(&path);
        w.append(frame("zero")).unwrap();
        w.sync().unwrap();
        let (mut reader, mut seen) = EventReader::open_path(&path).unwrap();
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"{\"agent_id\":\"a-1\"}\n").unwrap();
        }
        assert_eq!(reader.poll(&mut seen), 0);
        assert!(
            matches!(reader.status(), crate::registry::Status::Stopped { .. }),
            "{:?}",
            reader.status()
        );
        // …and it stays stopped: a later valid append is not narrated past the bytes marion
        // refused to read.
        w.append(frame("after")).unwrap();
        w.sync().unwrap();
        assert_eq!(reader.poll(&mut seen), 0);
        assert_eq!(seen.len(), 1);
    }

    /// A file that appears **after** a client attached is the normal case, not a fault: the node's
    /// process may not have written its first frame yet.
    #[test]
    fn a_stream_that_appears_after_the_reader_did_is_followed_rather_than_missed() {
        let dir = scratch("events-late");
        let path = dir.join("events.jsonl");
        let (mut reader, mut seen) = EventReader::open_path(&path).unwrap();
        assert!(!reader.ever_written());
        let mut w = writer(&path);
        w.append(frame("zero")).unwrap();
        w.sync().unwrap();
        assert_eq!(reader.poll(&mut seen), 1);
        assert!(
            reader.ever_written(),
            "and it is on record that it exists now"
        );
        assert_eq!(seen[0].agent_seq, 0);
    }

    // --- the sink, and §5.2's MUST -----------------------------------------------------------

    fn sink_over(path: &std::path::Path) -> EventSink {
        EventSink::new(
            EventWriter::open_path(path, &node()).unwrap(),
            Harness::ClaudeCode,
            "marion-init-a-1".into(),
        )
    }

    /// **§5.2's MUST, end to end.** The `initialize` reply is ~30 kB of the operator's slash-command
    /// catalogue, model prices, `account.tokenSource` and the CLI's pid. An events file is a journal
    /// in every sense that MUST cares about, so the body must not reach disk.
    #[test]
    fn the_initialize_reply_is_withheld_because_section_5_2_forbids_journaling_it_verbatim() {
        let dir = scratch("events-withheld");
        let path = dir.join("events.jsonl");
        let catalogue = "s".repeat(30_000);
        // Sentinel *values*, not key names: the withheld marker's own `reason` text names the keys
        // §5.2 is about, so asserting on those would be the test matching marion's prose about the
        // hazard rather than the hazard.
        let reply = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "marion-init-a-1",
                "response": {
                    "commands": catalogue,
                    "account": {"tokenSource": "SENTINEL-account-source"},
                    "pid": "SENTINEL-cli-pid",
                },
            }
        });
        {
            let s = sink_over(&path);
            s.record(StreamEvent::Frame(&reply));
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("SENTINEL-account-source") && !raw.contains("SENTINEL-cli-pid"),
            "the reply's body reached disk; §5.2 forbids exactly this"
        );
        assert!(!raw.contains(&catalogue), "nor the command catalogue");
        assert!(raw.len() < 1_000, "{} bytes written", raw.len());

        let (_, events) = read(&path);
        match &events[0].payload {
            Payload::Withheld { key, bytes, reason } => {
                assert_eq!(key, "control_response");
                assert!(*bytes > 30_000, "{bytes}");
                assert!(reason.contains("5.2"), "{reason}");
            }
            other => panic!("a withheld frame must still be an event, not a hole: {other:?}"),
        }
        assert_eq!(events[0].provenance.completeness, Completeness::Partial);
    }

    /// **The negative control against over-withholding.** §5.2 names the reply to *`initialize`*,
    /// not every `control_response`: S11's interrupt reply is 100 bytes of `{"still_queued":[]}` and
    /// is exactly the kind of thing a client re-attaching wants to see. A rule that swallowed the
    /// whole frame *type* would silently discard it.
    #[test]
    fn a_control_response_that_is_not_the_initialize_reply_is_kept_whole() {
        let dir = scratch("events-not-withheld");
        let path = dir.join("events.jsonl");
        let interrupt = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "marion-interrupt-7",
                "response": {"still_queued": []},
            }
        });
        {
            let s = sink_over(&path);
            s.record(StreamEvent::Frame(&interrupt));
        }
        let (_, events) = read(&path);
        match &events[0].payload {
            Payload::Vendor { key, json, .. } => {
                assert_eq!(key, "control_response");
                assert_eq!(json, &interrupt, "carried verbatim");
            }
            other => panic!("{other:?}"),
        }
    }

    /// **The sink meters the node's spend from the frames it records**, on every recording seam,
    /// under the harness's own rule: goose's terminal `complete` totals its process, so a second
    /// `result` in one process supersedes the first while a continuation's adds to it; a line that
    /// is not JSON is not a unit; and a harness whose row states no usage claims none — `None`,
    /// never zero. Each frame that moves the figure publishes it, at once, to the owner's live
    /// figures; one that does not publishes nothing.
    #[test]
    fn a_sink_meters_its_nodes_usage_across_generations_from_the_frames_it_records() {
        let dir = scratch("events-usage");
        let result = |input: u64, output: u64| {
            format!(
                "{{\"type\":\"complete\",\"input_tokens\":{input},\
                 \"output_tokens\":{output}}}"
            )
        };
        let spending = std::sync::Arc::new(crate::spending::Spending::default());
        let mut s = EventSink::new(
            EventWriter::open_path(&dir.join("goose.jsonl"), &node()).unwrap(),
            Harness::Goose,
            "unused".into(),
        )
        .publishing_to(&node(), Some(spending.clone()));
        assert_eq!(s.spent().usage, None, "nothing read, nothing claimed");
        s.record_line("not json");
        assert_eq!(spending.get(&node()), None, "no unit, nothing published");
        s.record_line(&result(1, 1));
        s.record_capture(&result(100, 20));
        assert_eq!(
            spending.get(&node()).and_then(|p| p.usage).map(|u| u.input),
            Some(100),
            "published the moment the frame was recorded"
        );
        s.end_generation();
        s.record_line(&result(30, 2));
        let u = s.spent().usage.expect("three units were recorded");
        assert_eq!((u.input, u.output), (130, 22));
        assert_eq!(
            spending.get(&node()),
            Some(s.spent()),
            "one figure, two readers"
        );

        let silent = EventSink::new(
            EventWriter::open_path(&dir.join("copilot.jsonl"), &node()).unwrap(),
            Harness::Copilot,
            "unused".into(),
        );
        silent.record_line(r#"{"type":"result","usage":{"premiumRequests":1}}"#);
        assert_eq!(
            silent.spent().usage,
            None,
            "copilot's row reads no token count"
        );
    }

    /// **The model a node ran is the one its stream names**, the latest naming winning, and a row
    /// that reads no model names none.
    #[test]
    fn a_sink_keeps_the_model_its_nodes_stream_says_is_running() {
        let dir = scratch("events-model");
        let mut s = EventSink::new(
            EventWriter::open_path(&dir.join("claude.jsonl"), &node()).unwrap(),
            Harness::ClaudeCode,
            "unused".into(),
        );
        assert_eq!(s.model(), None);
        s.record_line(r#"{"type":"system","subtype":"init","model":"claude-haiku-4-5-20251001"}"#);
        s.record_line(r#"{"type":"assistant","message":{"model":"ignored"}}"#);
        assert_eq!(s.model().as_deref(), Some("claude-haiku-4-5-20251001"));
        s.record_capture(r#"{"type":"system","subtype":"init","model":"claude-sonnet-5"}"#);
        assert_eq!(
            s.model().as_deref(),
            Some("claude-sonnet-5"),
            "the latest names it"
        );

        let codex = EventSink::new(
            EventWriter::open_path(&dir.join("codex.jsonl"), &node()).unwrap(),
            Harness::Codex,
            "unused".into(),
        );
        codex.record_line(r#"{"type":"thread.started","model":"gpt"}"#);
        assert_eq!(codex.model(), None, "codex's row reads no model");
    }

    /// A capture read after the process died is not a live observation, and §4.1 has an axis that
    /// says so. A path that recovers its frames from a finished capture (ACP's transcript, a
    /// launch-only root) records them honestly `observed_live: false` rather than falsely live.
    #[test]
    fn events_recovered_from_a_finished_capture_do_not_claim_to_have_been_observed_live() {
        let dir = scratch("events-capture");
        let path = dir.join("events.jsonl");
        let stdout = "{\"type\":\"thread.started\"}\nReading additional input from stdin...\n\
                      {\"type\":\"turn.completed\"}\n";
        {
            let mut s = EventSink::new(
                EventWriter::open_path(&path, &node()).unwrap(),
                Harness::Codex,
                "unused".into(),
            );
            s.record_capture(stdout);
        }
        let (log, events) = read(&path);
        assert_eq!(
            log.records, 3,
            "two frames and the non-JSON line between them"
        );
        assert!(
            events.iter().all(|e| !e.provenance.observed_live),
            "read after the fact, and the provenance must say so"
        );
        match &events[1].payload {
            Payload::Raw(s) => assert_eq!(s, "Reading additional input from stdin..."),
            other => panic!("a non-JSON line is still something the node said: {other:?}"),
        }
    }

    /// **A line recorded as it lands is the same event a capture would have made, marked live.**
    /// The launch-only child path records through this while the child runs, so its file says what
    /// the child has done before the child ends — the one line rule, and a trailing CR or LF is not
    /// part of the frame.
    #[test]
    fn a_line_recorded_as_it_lands_is_the_captures_event_observed_live() {
        let dir = scratch("events-line");
        let path = dir.join("events.jsonl");
        {
            let s = EventSink::new(
                EventWriter::open_path(&path, &node()).unwrap(),
                Harness::Codex,
                "unused".into(),
            );
            for line in [
                "{\"type\":\"thread.started\"}\r",
                "",
                "Reading additional input from stdin...\n",
                "{\"type\":\"turn.completed\"}",
            ] {
                s.record_line(line);
            }
        }
        let (log, events) = read(&path);
        assert_eq!(
            log.records, 3,
            "two frames and the raw line; a blank line is nothing"
        );
        assert!(events.iter().all(|e| e.provenance.observed_live));
        match (&events[0].payload, &events[1].payload) {
            (Payload::Vendor { key, .. }, Payload::Raw(s)) => {
                assert_eq!(key, "thread.started");
                assert_eq!(s, "Reading additional input from stdin...");
            }
            other => panic!("{other:?}"),
        }
    }

    /// **An endpoint node's key never reaches its live stream**: a duplex frame or raw line that
    /// echoes the key — an SDK error quoting the header it sent — is recorded with the key
    /// replaced, as it arrives, not only in the capture kept after the run.
    #[test]
    fn a_sink_given_the_nodes_key_scrubs_it_from_every_live_event() {
        let key = "sk-endpoint-live-0001";
        let dir = scratch("events-scrub");
        let path = dir.join("events.jsonl");
        {
            let s = EventSink::new(
                EventWriter::open_path(&path, &node()).unwrap(),
                Harness::ClaudeCode,
                "unused".into(),
            )
            .scrubbing(Some(key));
            let frame =
                serde_json::json!({"type": "assistant", "error": format!("401 for Bearer {key}")});
            s.record(StreamEvent::Frame(&frame));
            s.record(StreamEvent::Unparsed(&format!("x-api-key: {key}")));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(key), "{text}");
        assert_eq!(text.matches("***").count(), 2, "{text}");
        let (log, _) = read(&path);
        assert_eq!(log.records, 2, "both still recorded, and still parse");
    }

    /// Set on the re-executed copy of this binary that actually drives a node with a recording sink.
    const SINK_PROBE: &str = "MARION_EVENTS_SINK_STDOUT_PROBE";
    /// What the stub node says, and what marion must never repeat on its own streams.
    const SENTINEL: &str = "sentinel-9c3e-the-node-said-this";

    /// **NC — lifting `sink: None` did not put a byte of a node's stream on marion's own stdout.**
    ///
    /// `run_spawn` held `sink: None` because it runs inside `marion-supervisor`, whose stdout *is*
    /// the stdio MCP stream the root harness parses. The wiring lifts that, and the argument for why
    /// it is safe — an `EventSink` writes to a file — is exactly the kind of reasoning that is true
    /// until someone adds a `println!` to a frame handler for debugging. So it is asserted against a
    /// run that **has** a sink, rather than inferred.
    ///
    /// It cannot be checked in-process: libtest captures `println!` from the test thread, so an
    /// in-process assertion would pass against a driver that prints unconditionally. The driver
    /// therefore runs in a re-executed copy of this binary with `--nocapture`, where a stray write
    /// reaches a real pipe. Same mechanism as
    /// `duplex::tests::a_child_run_writes_not_one_byte_of_the_nodes_stream_to_marions_own_stdout`,
    /// which proves the `None` case; this proves the case marion now actually takes.
    #[test]
    fn a_child_run_with_a_recording_sink_still_writes_not_one_byte_to_marions_own_stdout() {
        if std::env::var(SINK_PROBE).is_ok() {
            let dir = scratch("events-sink-probe");
            let path = dir.join("events.jsonl");
            let marker = dir.join("mcp-ready");
            std::fs::write(&marker, b"ready\n").unwrap();
            let script = format!(
                r#"read init
printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"marion-init-probe","response":{{}}}}}}\n'
read user
printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{SENTINEL}"}}]}}}}\n'
printf 'this line is not json at all: {SENTINEL}\n'
printf '{{"type":"result","subtype":"success","result":"{SENTINEL}"}}\n'"#
            );
            let es = EventSink::new(
                EventWriter::open_path(&path, &node()).unwrap(),
                Harness::ClaudeCode,
                "marion-init-probe".into(),
            );
            let record = |ev: StreamEvent<'_>| es.record(ev);
            let out = crate::duplex::run_duplex(
                std::process::Command::new("sh").args(["-c", &script]),
                &crate::duplex::DuplexSpec {
                    ready_file: &marker,
                    prompt: "do the task",
                    init_id: "marion-init-probe".into(),
                    mcp_ready_timeout: Duration::from_secs(5),
                    depth: crate::depth::ROOT_DEPTH + 1,
                    wall_clock: Some(Duration::from_secs(30)),
                    sink: Some(&record),
                    on_started: None,
                    turns: None,
                    stop_on: None,
                    dialect: crate::duplex::Dialect::StreamJson,
                    abort: marion_harness::spec::AbortVerb::None { note: "test" },
                },
            )
            .expect("the run returns");
            drop(es);
            // **Both halves, or the silence proves nothing.** The stub must really have spoken, and
            // the sink must really have recorded it — a sink that silently did nothing would pass a
            // stdout-silence assertion trivially.
            assert!(out.stdout.contains(SENTINEL), "the stub never spoke");
            let recorded = std::fs::read_to_string(&path).unwrap();
            assert!(
                recorded.contains(SENTINEL),
                "the sink recorded nothing, so the silence below is vacuous"
            );
            return;
        }
        let exe = std::env::current_exe().expect("the test binary re-executes itself");
        let name = format!(
            "{}::a_child_run_with_a_recording_sink_still_writes_not_one_byte_to_marions_own_stdout",
            module_path!().split_once("::").expect("crate::module").1
        );
        let probe = std::process::Command::new(&exe)
            .args(["--exact", "--nocapture", "--test-threads", "1", &name])
            .env(SINK_PROBE, "1")
            .output()
            .expect("the probe runs");
        let stdout = String::from_utf8_lossy(&probe.stdout);
        let stderr = String::from_utf8_lossy(&probe.stderr);
        assert!(
            probe.status.success(),
            "the probe itself failed, so it proves nothing:\n{stdout}\n{stderr}"
        );
        assert!(
            !stdout.contains(SENTINEL),
            "recording a node's stream put it on marion's own stdout — the stdio MCP stream the \
             root harness parses. An `EventSink` must write to its file and nowhere else.\n{stdout}"
        );
        assert!(
            !stderr.contains(SENTINEL),
            "recording a node's stream put it on marion's stderr\n{stderr}"
        );
    }

    #[test]
    fn a_frame_is_never_marions_own_observation() {
        let v = json!({"type": "result"});
        let d = from_stream_event(Harness::ClaudeCode, StreamEvent::Frame(&v));
        assert_eq!(d.provenance.source, Source::Protocol);
        assert!(
            !d.provenance.authoritative,
            "the harness's transcript is its system of record, not marion's reading of its pipe"
        );
        assert!(d.provenance.observed_live);
        assert_eq!(
            Draft::marion(Payload::Lifecycle(Lifecycle::Opened)).provenance,
            Provenance::marion()
        );
    }
}
