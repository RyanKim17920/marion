//! **Reading a digest for a query**: newest line first, in 64 KiB blocks from the end.
//!
//! Each line is first tested on its raw bytes for every word at once (`aho-corasick`, ASCII case
//! folded, the words spelled as JSON escapes them). Only a line that holds every word is parsed,
//! and then each word is confirmed in the fields the query searches, so a word that only appears
//! as a key name or in a field out of scope does not match.
//!
//! **One run, one current line.** A resume, or a run appended and then rebuilt, leaves several
//! lines for one `agent_id`. The line with the greatest `ended` is the run, `at` breaking a tie,
//! whatever order they were appended in. Every line's id and end are read off its bytes (an exact
//! key, as the digest writes them), so an older line is skipped once a newer one was seen, and a
//! line found later that ended later replaces an earlier hit. The scan stops at the limit.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use aho_corasick::AhoCorasick;
use marion_core::search::{Query, State, find_folded};

use super::Skipped;
use super::digest::Run;

/// How much of the file one read takes.
const BLOCK: usize = 64 * 1024;

/// The longest line the scan carries across blocks; longer is not a line marion wrote.
const MAX_CARRY: usize = 1024 * 1024;

/// Characters of context a snippet keeps around its match.
const SNIPPET: usize = 60;
const LEAD: usize = 20;

/// The words a line skipped for having no id, or not parsing, is counted under.
const UNREADABLE: &str = "a torn or unreadable digest line";

/// One run that matched, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub run: Run,
    /// The field the first word was found in; empty for a query with no words.
    pub field: &'static str,
    /// About [`SNIPPET`] characters of that field around the match, whitespace folded to spaces.
    pub snippet: String,
}

/// What a scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scanned {
    /// Newest first.
    pub hits: Vec<Hit>,
    /// Runs looked at.
    pub searched: usize,
    pub skipped: Skipped,
}

/// A query made ready to test lines.
pub struct Matcher {
    query: Query,
    /// Every word as JSON spells it, when every word can be tested on bytes.
    prefilter: Option<AhoCorasick>,
}

impl Matcher {
    pub fn new(query: &Query) -> Matcher {
        let ascii = query.terms.iter().all(|t| t.is_ascii());
        let patterns: Vec<String> = query
            .terms
            .iter()
            .map(|t| {
                let quoted = serde_json::to_string(t).expect("a string always serializes");
                quoted[1..quoted.len() - 1].to_string()
            })
            .collect();
        // A word with a non-ASCII letter is folded by `to_lowercase` on the confirm step only,
        // so the byte test would refuse a line that matches; such a query is confirmed whole.
        let prefilter = (ascii && !patterns.is_empty())
            .then(|| {
                AhoCorasick::builder()
                    .ascii_case_insensitive(true)
                    .build(&patterns)
                    .ok()
            })
            .flatten();
        Matcher {
            query: query.clone(),
            prefilter,
        }
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    /// Whether `line` holds every word anywhere in its bytes.
    fn may_match(&self, line: &[u8]) -> bool {
        let Some(ac) = &self.prefilter else {
            return true;
        };
        let want = self.query.terms.len();
        let mut seen = vec![false; want];
        let mut found = 0;
        for m in ac.find_overlapping_iter(line) {
            let i = m.pattern().as_usize();
            if !seen[i] {
                seen[i] = true;
                found += 1;
                if found == want {
                    return true;
                }
            }
        }
        false
    }

    /// **Whether `run` matches**, and its snippet: every filter holds, and every word is in a
    /// field the query searches.
    pub fn confirm(&self, run: &Run) -> Option<Hit> {
        self.confirm_in(run, None)
    }

    /// [`Self::confirm`] for a run that is live in `state` (`running` or `waiting`), which no
    /// digest line records: a live row built from the tree.
    pub fn confirm_live(&self, run: &Run, state: State) -> Option<Hit> {
        self.confirm_in(run, Some(state))
    }

    fn confirm_in(&self, run: &Run, live: Option<State>) -> Option<Hit> {
        if !self.filters_hold(run, live) {
            return None;
        }
        let fields = fields(run, &self.query);
        let mut first: Option<(&'static str, String)> = None;
        for term in &self.query.terms {
            let (name, text, at) = fields
                .iter()
                .find_map(|(name, text)| find_folded(text, term).map(|at| (*name, text, at)))?;
            if first.is_none() {
                first = Some((name, snippet(text, at)));
            }
        }
        let (field, snippet) = first.unwrap_or_else(|| ("", snippet(&run.report, 0)));
        Some(Hit {
            run: run.clone(),
            field,
            snippet,
        })
    }

    fn filters_hold(&self, run: &Run, live: Option<State>) -> bool {
        let q = &self.query;
        let lower = |s: &str| s.to_lowercase();
        let any = |want: &[String], has: &dyn Fn(&str) -> bool| {
            want.is_empty() || want.iter().any(|w| has(w))
        };
        let ended = run.ended.unwrap_or(run.at);
        let ended_ms = ended
            .0
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        any(&q.harness, &|w| lower(&run.harness) == w)
            && any(&q.model, &|w| {
                run.model.as_deref().is_some_and(|m| lower(m).contains(w))
            })
            && any(&q.agent_type, &|w| lower(&run.agent_type) == w)
            && any(&q.id, &|w| id_matches(run, w))
            && (q.state.is_empty()
                || q.state.iter().any(|s| match live {
                    Some(now) => *s == now,
                    None => state_holds(run, *s),
                }))
            && q.since.is_none_or(|s| ended_ms >= s)
            && q.until.is_none_or(|u| ended_ms < u)
    }
}

/// Whether `run`, which ended, is in `state`.
fn state_holds(run: &Run, state: State) -> bool {
    match state {
        State::Landed => run.landed,
        State::Running | State::Waiting => false,
        s => run.status.map(State::of_exit) == Some(s),
    }
}

/// `want` is the start of the run's agent or task id, or its short id.
fn id_matches(run: &Run, want: &str) -> bool {
    run.agent_id.0.starts_with(want)
        || run.task_id.as_ref().is_some_and(|t| t.0.starts_with(want))
        || crate::node_label::short_id(&run.agent_id.0) == want
}

/// The fields `query` searches, by name, in the order a snippet prefers them.
fn fields(run: &Run, query: &Query) -> Vec<(&'static str, String)> {
    let s = query.scopes;
    let mut out = Vec::new();
    if s.prompt {
        out.push(("prompt", run.prompt.clone()));
    }
    if s.report {
        out.push(("report", run.report.clone()));
        for c in &run.checks {
            let verdict = if c.ok { "passed" } else { "failed" };
            out.push(("checks", format!("{} {verdict}", c.cmd)));
        }
        for f in &run.findings {
            out.push(("findings", f.clone()));
        }
    }
    if s.paths {
        for p in &run.paths {
            out.push(("paths", p.clone()));
        }
    }
    if s.paths || s.meta {
        out.extend(run.branch.clone().map(|b| ("branch", b)));
    }
    if s.meta {
        out.push(("id", run.agent_id.0.clone()));
        out.extend(run.task_id.as_ref().map(|t| ("task", t.0.clone())));
        out.push(("type", run.agent_type.clone()));
        out.push(("harness", run.harness.clone()));
        out.extend(run.model.clone().map(|m| ("model", m)));
        out.extend(
            run.status
                .map(|st| ("state", State::of_exit(st).word().to_string())),
        );
    }
    out
}

/// About [`SNIPPET`] characters of `text` from [`LEAD`] before byte `at`, with `…` where it was
/// cut and every run of whitespace one space, so a snippet is one line.
fn snippet(text: &str, at: usize) -> String {
    let start_char = text[..at.min(text.len())]
        .chars()
        .count()
        .saturating_sub(LEAD);
    let mut out = String::new();
    if start_char > 0 {
        out.push('…');
    }
    let mut space = false;
    let mut cut = false;
    for (kept, c) in text.chars().skip(start_char).enumerate() {
        if kept == SNIPPET {
            cut = true;
            break;
        }
        if c.is_whitespace() {
            if !space && !out.is_empty() && !out.ends_with('…') {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    let mut out = out.trim_end().to_string();
    if cut {
        out.push('…');
    }
    out
}

/// The string value of `"key":"…"` in a digest line's bytes, as the digest writes it; `None` for
/// a missing key or a `null`.
fn value_of<'a>(line: &'a [u8], key: &memchr::memmem::Finder<'_>) -> Option<&'a [u8]> {
    let at = key.find(line)? + key.needle().len();
    let rest = &line[at..];
    let end = memchr::memchr(b'"', rest)?;
    Some(&rest[..end])
}

/// Per agent: the `ended` and `at` of its newest-ended line seen, and its hit's index if that line
/// matched.
type Newest = (Vec<u8>, Vec<u8>, Option<usize>);

/// A rebuilt digest's header line, which names no run.
fn is_header(line: &[u8]) -> bool {
    line.starts_with(br#"{"v":"#) && memchr::memmem::find(line, br#""built":"#).is_some()
}

/// **Scan `path` for `matcher`'s query**, newest line first, stopping once `limit` runs match.
pub fn scan(path: &Path, matcher: &Matcher, limit: usize) -> io::Result<Scanned> {
    let mut file = super::open::open_regular(path)?;
    let mut lines = Reverse::new(&mut file)?;
    let id_key = memchr::memmem::Finder::new(br#""agent_id":""#);
    let ended_key = memchr::memmem::Finder::new(br#""ended":""#);
    let at_key = memchr::memmem::Finder::new(br#""at":""#);
    let mut seen: HashMap<Vec<u8>, Newest> = HashMap::new();
    let mut out = Scanned::default();
    let mut hits: Vec<Option<Hit>> = Vec::new();
    let mut matched = 0;
    while matched < limit {
        let Some(line) = lines.next_line()? else {
            break;
        };
        if line.is_empty() || is_header(&line) {
            continue;
        }
        // A short write leaves a line cut before its closing brace; it names no run, and must not
        // stand in for the whole line before it.
        let whole = line.ends_with(b"}");
        let Some(id) = value_of(&line, &id_key).filter(|_| whole) else {
            out.skipped.note(UNREADABLE);
            continue;
        };
        let order = (
            value_of(&line, &ended_key).unwrap_or_default().to_vec(),
            value_of(&line, &at_key).unwrap_or_default().to_vec(),
        );
        let replaces = match seen.get(id) {
            Some((e, a, _)) if (e, a) >= (&order.0, &order.1) => continue,
            Some((_, _, earlier)) => Some(*earlier),
            None => None,
        };
        if replaces.is_none() {
            out.searched += 1;
        }
        let hit = if matcher.may_match(&line) {
            match serde_json::from_slice::<Run>(&line) {
                Ok(run) => matcher.confirm(&run),
                Err(_) => {
                    out.skipped.note(UNREADABLE);
                    None
                }
            }
        } else {
            None
        };
        // A later end found further back: the earlier hit was not the run's current line.
        if let Some(Some(i)) = replaces
            && hits[i].take().is_some()
        {
            matched -= 1;
        }
        let index = hit.map(|h| {
            hits.push(Some(h));
            matched += 1;
            hits.len() - 1
        });
        seen.insert(id.to_vec(), (order.0, order.1, index));
    }
    out.hits = hits.into_iter().flatten().collect();
    Ok(out)
}

/// A file's lines from its end, one block at a time.
struct Reverse<'f> {
    file: &'f mut File,
    /// Bytes of the file not yet read: `[0, pos)`.
    pos: u64,
    /// Whole lines read, in file order; taken from the back.
    ready: Vec<Vec<u8>>,
    /// The start of the earliest line read, which may continue in the block before.
    carry: Vec<u8>,
}

impl<'f> Reverse<'f> {
    fn new(file: &'f mut File) -> io::Result<Self> {
        let pos = file.seek(SeekFrom::End(0))?;
        Ok(Reverse {
            file,
            pos,
            ready: Vec::new(),
            carry: Vec::new(),
        })
    }

    fn next_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(line) = self.ready.pop() {
                return Ok(Some(line));
            }
            if self.pos == 0 {
                if self.carry.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(std::mem::take(&mut self.carry)));
            }
            let start = self.pos.saturating_sub(BLOCK as u64);
            let mut block = vec![0; (self.pos - start) as usize];
            self.file.seek(SeekFrom::Start(start))?;
            self.file.read_exact(&mut block)?;
            self.pos = start;
            block.extend_from_slice(&self.carry);
            let mut parts: Vec<Vec<u8>> =
                block.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
            self.carry = parts.remove(0);
            if self.carry.len() > MAX_CARRY {
                // Not a line marion wrote; dropped rather than held whole.
                self.carry.clear();
            }
            self.ready = parts;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::digest::Check;
    use crate::search::digest::tests::run;
    use marion_core::contract::ExitStatus;
    use marion_core::encoding::SystemTime;
    use marion_testsupport::scratch;

    const NOW: u64 = 1_792_108_800_000;

    fn q(text: &str) -> Matcher {
        Matcher::new(&Query::parse(text, NOW).unwrap())
    }

    fn write(path: &Path, runs: &[Run]) {
        let mut bytes = Vec::new();
        for r in runs {
            bytes.extend(serde_json::to_vec(r).unwrap());
            bytes.push(b'\n');
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn ids(s: &Scanned) -> Vec<String> {
        s.hits.iter().map(|h| h.run.agent_id.0.clone()).collect()
    }

    fn ended_at(mut r: Run, ms: u64) -> Run {
        r.ended = Some(SystemTime::from_unix_millis(ms));
        r
    }

    #[test]
    fn hits_come_newest_first_and_stop_at_the_limit() {
        let dir = scratch("scan-order");
        let path = dir.join("runs.jsonl");
        let runs: Vec<Run> = (0..5).map(|i| run(&format!("r{i}"))).collect();
        write(&path, &runs);
        let all = scan(&path, &q("limiter"), 20).unwrap();
        assert_eq!(ids(&all), ["r4", "r3", "r2", "r1", "r0"]);
        assert_eq!(all.searched, 5);
        let two = scan(&path, &q("limiter"), 2).unwrap();
        assert_eq!(ids(&two), ["r4", "r3"]);
        assert_eq!(two.searched, 2, "an early stop reads no further");
    }

    /// Lines across many blocks read whole: the carry joins a line cut by a block boundary.
    #[test]
    fn lines_cut_by_block_boundaries_read_whole() {
        let dir = scratch("scan-blocks");
        let path = dir.join("runs.jsonl");
        let runs: Vec<Run> = (0..300)
            .map(|i| {
                let mut r = run(&format!("r{i}"));
                r.report = format!("{} needle-{i}", "x".repeat(700));
                r
            })
            .collect();
        write(&path, &runs);
        let found = scan(&path, &q("needle-7"), 500).unwrap();
        let want: Vec<String> = (0..300)
            .rev()
            .filter(|i| i.to_string().starts_with('7'))
            .map(|i| format!("r{i}"))
            .collect();
        assert_eq!(ids(&found), want, "r7 and r70..r79, newest first");
        assert_eq!(found.searched, 300);
        assert!(found.skipped.0.is_empty());
    }

    #[test]
    fn words_and_ignore_case_and_need_every_word() {
        let dir = scratch("scan-words");
        let path = dir.join("runs.jsonl");
        let mut a = run("a");
        a.prompt = "Add a Token-Bucket limiter".into();
        let mut b = run("b");
        b.prompt = "add a leaky bucket".into();
        b.report = "no limiter here".into();
        write(&path, &[a, b]);
        assert_eq!(ids(&scan(&path, &q("token-bucket"), 9).unwrap()), ["a"]);
        assert_eq!(
            ids(&scan(&path, &q("bucket limiter"), 9).unwrap()),
            ["b", "a"]
        );
        assert_eq!(
            ids(&scan(&path, &q(r#""leaky bucket""#), 9).unwrap()),
            ["b"]
        );
        assert!(scan(&path, &q("bucket absent"), 9).unwrap().hits.is_empty());
    }

    /// A word that is only a key name in the line's JSON does not match.
    #[test]
    fn a_key_name_is_not_a_match() {
        let dir = scratch("scan-key-name");
        let path = dir.join("runs.jsonl");
        write(&path, &[run("a")]);
        assert!(
            scan(&path, &q("findings_omitted"), 9)
                .unwrap()
                .hits
                .is_empty()
        );
        assert!(scan(&path, &q("agent_type"), 9).unwrap().hits.is_empty());
    }

    #[test]
    fn scopes_choose_the_fields() {
        let dir = scratch("scan-scopes");
        let path = dir.join("runs.jsonl");
        let mut a = run("a");
        a.paths = vec!["src/limits/webhook.rs".into()];
        a.checks = vec![Check {
            cmd: "cargo test -q limits".into(),
            ok: false,
        }];
        write(&path, &[a]);
        let hit = &scan(&path, &q("webhook"), 9).unwrap().hits[0];
        assert_eq!(
            (hit.field, hit.snippet.as_str()),
            ("paths", "src/limits/webhook.rs")
        );
        assert!(
            scan(&path, &q("webhook in:prompt"), 9)
                .unwrap()
                .hits
                .is_empty()
        );
        let check = &scan(&path, &q("limits in:report"), 9).unwrap().hits[0];
        assert_eq!(check.field, "checks");
        assert_eq!(check.snippet, "cargo test -q limits failed");
        assert_eq!(scan(&path, &q("codex in:meta"), 9).unwrap().hits.len(), 1);
        assert!(
            scan(&path, &q("codex in:prompt"), 9)
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn filters_narrow() {
        let dir = scratch("scan-filters");
        let path = dir.join("runs.jsonl");
        let mut a = ended_at(run("019fbf94-3c33-7000-8000-000000000001"), NOW - 3_600_000);
        a.harness = "claude".into();
        a.model = Some("claude-sonnet-5".into());
        a.landed = true;
        let mut b = ended_at(run("b"), NOW - 3 * 86_400_000);
        b.status = Some(ExitStatus::Failed);
        write(&path, &[a, b]);
        let one = |text: &str| ids(&scan(&path, &q(text), 9).unwrap());
        assert_eq!(
            one("harness:claude"),
            ["019fbf94-3c33-7000-8000-000000000001"]
        );
        assert_eq!(one("harness:codex harness:claude").len(), 2);
        assert_eq!(one("model:sonnet").len(), 1);
        assert_eq!(one("state:failed"), ["b"]);
        assert_eq!(one("state:landed").len(), 1);
        assert_eq!(one("state:ok").len(), 1, "landed is ok too");
        assert!(one("state:running").is_empty(), "an ended run is not live");
        assert_eq!(one("since:1d").len(), 1);
        assert_eq!(one("until:2d"), ["b"]);
        assert_eq!(one("id:3c33").len(), 1, "the short id");
        assert_eq!(one("id:019fbf94").len(), 1, "a prefix");
    }

    /// **The greatest `ended` wins over append order**: a resume's line written before the first
    /// run's (two ends landing out of order) is still the run.
    #[test]
    fn the_greatest_end_wins_whatever_the_append_order() {
        let dir = scratch("scan-greatest-end");
        let path = dir.join("runs.jsonl");
        let mut resumed = ended_at(run("a"), NOW);
        resumed.status = Some(ExitStatus::Failed);
        let first = ended_at(run("a"), NOW - 60_000);
        write(&path, &[resumed.clone(), first.clone()]);
        let ok = scan(&path, &q("state:ok"), 9).unwrap();
        assert!(
            ok.hits.is_empty(),
            "the older end is not the run: {:?}",
            ok.hits
        );
        assert_eq!(ids(&scan(&path, &q("state:failed"), 9).unwrap()), ["a"]);
        write(&path, &[first, resumed]);
        assert!(scan(&path, &q("state:ok"), 9).unwrap().hits.is_empty());
        let failed = scan(&path, &q("state:failed"), 9).unwrap();
        assert_eq!(ids(&failed), ["a"]);
        assert_eq!(failed.searched, 1, "one run, two lines");
    }

    #[test]
    fn a_torn_line_is_skipped_and_counted() {
        let dir = scratch("scan-torn");
        let path = dir.join("runs.jsonl");
        let good = serde_json::to_vec(&run("a")).unwrap();
        let mut bytes = br#"{"v":1,"built":"2026-10-16T00:00:00.000Z"}"#.to_vec();
        bytes.push(b'\n');
        bytes.extend_from_slice(&good);
        bytes.push(b'\n');
        bytes.extend_from_slice(&good[..good.len() / 2]);
        bytes.push(b'\n');
        bytes.extend_from_slice(b"garbage\n");
        std::fs::write(&path, bytes).unwrap();
        let found = scan(&path, &q("limiter"), 9).unwrap();
        assert_eq!(ids(&found), ["a"]);
        assert_eq!(found.skipped.total(), 2, "{:?}", found.skipped);
    }

    #[test]
    fn a_non_ascii_word_matches_whatever_its_case() {
        let dir = scratch("scan-unicode");
        let path = dir.join("runs.jsonl");
        let mut a = run("a");
        a.prompt = "ÉCOLE limiter".into();
        write(&path, &[a]);
        assert_eq!(ids(&scan(&path, &q("école"), 9).unwrap()), ["a"]);
    }

    #[test]
    fn a_word_json_escapes_still_matches() {
        let dir = scratch("scan-escaped");
        let path = dir.join("runs.jsonl");
        let mut a = run("a");
        a.report = r#"said "done" at C:\work"#.into();
        write(&path, &[a]);
        assert_eq!(ids(&scan(&path, &q(r#""C:\work""#), 9).unwrap()), ["a"]);
    }

    #[test]
    fn a_snippet_is_one_line_around_the_match() {
        let text = format!(
            "{}\n\nthe limiter\tnow refuses {}",
            "a".repeat(40),
            "b".repeat(80)
        );
        let at = text.find("limiter").unwrap();
        let s = snippet(&text, at);
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
        assert!(s.contains("the limiter now refuses"), "{s}");
        assert!(!s.contains(['\n', '\t']));
        assert_eq!(snippet("short", 0), "short");
    }

    /// A FIFO where the digest goes is refused at once. Mutation: open it blocking and this hangs.
    #[test]
    fn a_fifo_digest_is_refused_without_hanging() {
        let dir = scratch("scan-fifo");
        let path = dir.join("runs.jsonl");
        crate::search::open::tests::mkfifo(&path);
        assert!(scan(&path, &q("limiter"), 9).is_err());
    }

    #[test]
    fn a_symlinked_digest_is_refused() {
        let dir = scratch("scan-symlink");
        let real = dir.join("real");
        write(&real, &[run("a")]);
        let link = dir.join("runs.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(scan(&link, &q("limiter"), 9).is_err());
    }
}
