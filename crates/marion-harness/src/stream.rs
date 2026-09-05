//! Reading a child's own output stream: framing (§5.2) and what the frames said (§6.1 step 9).
//!
//! > *"Any consumer of a harness's stdout MUST buffer across reads and split on frame boundaries.
//! > It MUST NOT treat a `read()` as a frame […] A stream-json reader MUST tolerate a trailing
//! > `\r`."*
//!
//! That MUST is normative and general, so [`FrameSplitter`] satisfies it *by construction* rather
//! than by the accident of how today's callers happen to read. marion's children currently run
//! over pipes and are drained whole before parsing, which is the easy case — but S11 measured 40%
//! of pty reads carrying no line terminator at all, and the day a harness moves to `spawn_pty` the
//! splitter must already be right. So the whole-string entry point ([`json_frames`]) is the *same*
//! splitter fed one chunk, not a second implementation over `str::lines`.
//!
//! What the splitter deliberately does **not** do is decide what a frame means. Non-JSON noise is
//! a per-harness hazard — S12 recorded `Warning: Basic terminal detected…` and `[STARTUP] Phase …`
//! interleaved on gemini's stdout — so filtering to lines that begin with `{` belongs to the
//! reader, and is done once in [`json_frames`] for every adapter.

use std::path::PathBuf;

use serde_json::Value;

/// What a child's **own output stream** said. Process-level facts are not here.
///
/// The split is deliberate: exit code, signal and "marion killed it" are things marion observed
/// about the process, whereas everything in this struct is something the child claimed. Keeping
/// them apart is what lets [`crate::HarnessAdapter::parse_stream`] be a pure function of bytes,
/// and it is why a harness that cannot supply a field leaves it empty instead of inventing one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamOutcome {
    /// The `narrative` the child passed to marion's `report` tool, in whatever spelling that
    /// harness gives the tool. `None` means the child never reported — never "it said nothing
    /// interesting" — and `build_contract` turns that into `Unreported`.
    pub narrative: Option<String>,
    /// Paths the harness *itself* announced editing. **Corroboration only: git is the authority**
    /// for `changed_paths`, so a harness that announces nothing costs marion no fidelity. Only
    /// codex emits such events; gemini and opencode have no equivalent frame and leave this empty.
    pub file_change_paths: Vec<PathBuf>,
    /// The `result_commits` the child passed to `report`, **exactly as it spelled them**.
    ///
    /// `Vec<String>` and not `Vec<Oid>` on purpose: at this layer these are a foreign agent's
    /// words, not object names marion has stood behind. §6.7 gives the child this field outright,
    /// so nothing here checks that an entry is well-formed, exists, or is reachable — see
    /// `Completion::result_commits`, which says the same thing where a reader of the contract will
    /// find it. An empty vec means the child named no commits, which is what an absent key means
    /// too: the field is optional in `report`'s schema.
    pub result_commits: Vec<String>,
    /// The stream said the run failed, whatever the exit code says.
    ///
    /// This exists because **the exit code is not sufficient on every harness**: S12 measured a
    /// gemini auth failure returning **exit 0 with a JSON error body**, so a reader that trusted
    /// the number would record a clean run. Recorded rather than acted on — §6.7's audit record
    /// carries the harness's own words, and `None` here never means "it succeeded", only "the
    /// stream itself made no failure claim".
    pub failure: Option<String>,
}

/// `result_commits` out of a `report` call's arguments — **one derivation, four wires**.
///
/// The four harnesses wrap the same arguments object under four different keys (`input`,
/// `arguments`, `parameters`, `part.state.input`), so each adapter locates the object and this
/// reads the field out of it. A per-adapter copy of these four lines is exactly the duplication
/// that lets one wire quietly drift into accepting something the others reject.
///
/// Anything that is not an array of strings yields an empty vec, which is also what an absent key
/// yields — deliberately, because `report`'s schema makes the field optional and codex's
/// `--output-schema` expresses that optionality as an explicit JSON `null` (§9). A child that
/// names no commits and a child that names the field as null are making the same claim.
///
/// **No entry is validated.** See `Completion::result_commits`.
pub fn report_commits(args: &Value) -> Vec<String> {
    args["result_commits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// One call to one of marion's verbs, as a harness's stream showed it — **and what came of it**.
///
/// The verb is in *marion's* vocabulary (`spawn`, `report`, …), never the harness's spelling: the
/// four harnesses name the same verb four ways (§3.1) and a caller comparing against `"spawn"` must
/// be right on all four.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarionCall {
    pub verb: String,
    pub outcome: CallOutcome,
}

/// What a harness's stream says became of one marion call.
///
/// **Three values and not two, because "no news" is its own answer.** A stream that shows a call
/// starting and never shows it ending is not a call that succeeded; folding [`Self::Unknown`] into
/// [`Self::Answered`] would be a reader reporting success by failing to look, which is the defect
/// class this repository keeps re-finding (`alive()` as `kill(pid,0)==0`, `survivors()` with
/// `.unwrap_or_default()`, and `report_refusal` serving on an unparsable depth until `36fbbee`).
/// Every one of the four measured event sets carries a terminal frame per call — codex's
/// `item.completed`, gemini's `tool_result`, opencode's terminal-only `tool_use`, Claude Code's
/// `tool_result` block — so `Unknown` means a stream that stopped mid-call, which is news.
///
/// **What this cannot see is stated here rather than discovered later.** `Refused` is populated
/// from the *harness's* structural error signal. A refusal marion's own bridge issued arrives as an
/// MCP result with `isError: true`, and whether each harness re-surfaces that as a stream-level
/// error is **unmeasured**: `tests/fixtures/s6/` records codex's `mcp_tool_call` item only with
/// `status: "completed"`, `error: null`, and S12 captured gemini's `tool_result` `status` only as
/// `"success"`. So a §5.4 authorization refusal *may* read as `Answered` on codex and gemini until
/// somebody records the failing shape. Matching marion's refusal prose in the result body would
/// close it today and is deliberately not done — a gate keyed on a sentence breaks the moment the
/// sentence is reworded, and every one of them was reworded in this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    /// The stream showed the call reaching a terminal state the harness did not mark as an error.
    Answered,
    /// The stream showed the call ending in an error, in the harness's own words.
    Refused(String),
    /// The stream showed the call and never showed its result.
    Unknown,
}

impl CallOutcome {
    /// The one question §6.1 step 8 asks of a call. A method rather than a `matches!` at each use
    /// site, so "what counts as an answer" is one decision and not one per caller.
    pub fn is_answered(&self) -> bool {
        matches!(self, Self::Answered)
    }
}

/// What marion observed about the child *process*, for the adapters whose success rule needs it.
///
/// Passed to [`crate::HarnessAdapter::parse_stream`] so each harness can state its own rule rather
/// than have the supervisor guess one for it — the codes are per-harness (gemini documents 0/1/42/
/// 53; opencode 1 for both "no message" and "unresolvable model") and, on gemini, are not decisive
/// on their own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChildExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// marion's own attributed kill (§6.7), not something the child reported.
    pub timed_out: bool,
}

/// A line-delimited frame splitter that buffers across reads.
///
/// Byte-oriented on purpose. A read boundary can fall in the middle of a multi-byte character —
/// nothing about a pipe or a pty respects UTF-8 — so the buffer holds bytes and decoding happens
/// only once a whole frame is in hand. Decoding each `read()` instead would corrupt exactly the
/// characters an agent's narrative is most likely to contain.
#[derive(Debug, Default)]
pub struct FrameSplitter {
    buf: Vec<u8>,
}

impl FrameSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one read's bytes. Returns every frame that this chunk **completed**, in order.
    ///
    /// A chunk carrying no terminator completes nothing and returns an empty vec — the bytes stay
    /// buffered. That is the whole point: a `read()` is not a frame.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(i) = self.buf[start..].iter().position(|b| *b == b'\n') {
            let end = start + i;
            out.push(decode(&self.buf[start..end]));
            start = end + 1;
        }
        self.buf.drain(..start);
        out
    }

    /// The trailing bytes at end-of-stream, if the child's last frame had no terminator.
    ///
    /// Called on **stdout close**, which for opencode is the only terminal signal there is: S13
    /// records that its stream carries no init, result or usage event and simply ends when the
    /// session goes idle. Dropping this tail would silently lose a whole frame on any harness that
    /// exits without a final newline.
    pub fn finish(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let frame = decode(&self.buf);
        self.buf.clear();
        (!frame.is_empty()).then_some(frame)
    }
}

/// One frame's bytes as text, with the CRLF half of a `\r\n` terminator removed.
///
/// §5.2's second MUST. `from_utf8_lossy` is safe here in a way it would not be per-`read()`: the
/// splitter never calls this until it holds the frame's whole byte range, so a replacement
/// character can only come from bytes that were genuinely not UTF-8.
fn decode(frame: &[u8]) -> String {
    String::from_utf8_lossy(frame)
        .trim_end_matches('\r')
        .to_string()
}

/// Every JSON **object** frame in a captured stdout, in order.
///
/// Two filters, each with a measurement behind it:
///
/// - **frames are split by [`FrameSplitter`]**, not by `str::lines`, so this entry point and an
///   incremental reader can never disagree about where a frame ends;
/// - **only lines beginning with `{` are parsed.** S12: gemini interleaves `Warning: Basic
///   terminal detected…` and `[STARTUP] Phase …` on the same stream as its NDJSON, so tolerating
///   non-JSON lines is a requirement rather than defensiveness. A line that begins with `{` but
///   does not parse is dropped just as quietly — a half-written frame is not evidence of anything.
pub fn json_frames(stdout: &str) -> Vec<Value> {
    let mut splitter = FrameSplitter::new();
    let mut lines = splitter.push(stdout.as_bytes());
    lines.extend(splitter.finish());
    lines
        .iter()
        .filter_map(|line| {
            let t = line.trim();
            if !t.starts_with('{') {
                return None;
            }
            serde_json::from_str::<Value>(t).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus every framing test below is run against: two real frames, a `\r\n` terminator, a
    /// non-JSON warning line of the family S12 recorded, and a multi-byte character *inside* a
    /// frame so that an adversarial split necessarily lands mid-character.
    const CORPUS: &str = "Warning: Basic terminal detected. Some features may not work.\n\
                          {\"type\":\"init\",\"model\":\"gemini-2.5-flash\"}\r\n\
                          [STARTUP] Phase 2\n\
                          {\"type\":\"message\",\"content\":\"héllo — 世界\"}\n";

    fn frames_from_chunks(s: &str, chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = FrameSplitter::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(splitter.push(c));
        }
        out.extend(splitter.finish());
        assert_eq!(
            chunks.iter().map(|c| c.len()).sum::<usize>(),
            s.len(),
            "the chunking must cover the whole input"
        );
        out
    }

    #[test]
    fn a_read_is_not_a_frame_and_every_split_point_yields_the_same_frames() {
        // §5.2's MUST, exercised at *every* byte boundary rather than at a chosen one: many of
        // these land mid-frame, and four land inside the multi-byte characters.
        let whole = frames_from_chunks(CORPUS, &[CORPUS.as_bytes()]);
        let bytes = CORPUS.as_bytes();
        for i in 0..=bytes.len() {
            let split = frames_from_chunks(CORPUS, &[&bytes[..i], &bytes[i..]]);
            assert_eq!(split, whole, "split at byte {i}");
        }
    }

    #[test]
    fn a_frame_split_mid_utf8_is_not_corrupted() {
        // The interesting case on its own, stated so a regression names itself: the splitter must
        // buffer bytes and decode whole frames, never decode each read.
        let text = "{\"content\":\"héllo — 世界\"}\n";
        let bytes = text.as_bytes();
        for i in 0..bytes.len() {
            let frames = frames_from_chunks(text, &[&bytes[..i], &bytes[i..]]);
            assert_eq!(frames, vec!["{\"content\":\"héllo — 世界\"}"], "at {i}");
            assert!(
                !frames[0].contains('\u{FFFD}'),
                "a replacement character at split {i} means a read was decoded, not a frame"
            );
        }
    }

    #[test]
    fn a_trailing_carriage_return_is_tolerated_on_every_frame() {
        let mut s = FrameSplitter::new();
        assert_eq!(
            s.push(b"{\"a\":1}\r\n{\"b\":2}\n"),
            vec!["{\"a\":1}", "{\"b\":2}"]
        );
        // And a bare `\r` frame is nothing, not an empty frame that a parser then has to skip.
        let mut s = FrameSplitter::new();
        assert!(s.push(b"\r\n").iter().all(String::is_empty));
    }

    #[test]
    fn a_final_frame_with_no_terminator_is_still_delivered_at_close() {
        // opencode's stream just ends when the session goes idle (`tests/fixtures/s13/`), so a
        // reader that waited for a terminator would lose the last frame — or hang for it.
        let mut s = FrameSplitter::new();
        assert!(
            s.push(b"{\"type\":\"text\"}").is_empty(),
            "no terminator yet"
        );
        assert_eq!(s.finish().as_deref(), Some("{\"type\":\"text\"}"));
        assert_eq!(s.finish(), None, "and the buffer is drained, not replayed");
    }

    #[test]
    fn interleaved_non_json_lines_are_skipped_rather_than_failing_the_parse() {
        // S12: warnings and `[STARTUP]` lines share the stream with the NDJSON.
        let frames = json_frames(CORPUS);
        assert_eq!(frames.len(), 2, "two JSON frames among four lines");
        assert_eq!(frames[0]["type"], "init");
        assert_eq!(frames[1]["content"], "héllo — 世界");
    }

    #[test]
    fn a_half_written_frame_is_dropped_rather_than_guessed_at() {
        let frames = json_frames("{\"type\":\"init\"}\n{\"type\":\"mess");
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn json_frames_agrees_with_the_incremental_reader_byte_for_byte() {
        // The two entry points are one implementation; this is what says so.
        let bytes = CORPUS.as_bytes();
        for i in 0..=bytes.len() {
            let mut s = FrameSplitter::new();
            let mut lines = s.push(&bytes[..i]);
            lines.extend(s.push(&bytes[i..]));
            lines.extend(s.finish());
            let parsed: Vec<Value> = lines
                .iter()
                .filter(|l| l.trim_start().starts_with('{'))
                .filter_map(|l| serde_json::from_str(l.trim()).ok())
                .collect();
            assert_eq!(parsed, json_frames(CORPUS), "split at {i}");
        }
    }
}
