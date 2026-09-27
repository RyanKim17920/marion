//! Cross-harness review: the policy a reviewer node runs under, and the pure rules marion uses to
//! turn a reviewer's text into a verdict.
//!
//! **marion decides the verdict, not the model.** A reviewer reports findings; marion blocks only
//! on a finding that is *grounded* (it names a file the change actually touched) and at least as
//! severe as the spec's threshold. A reviewer that says "block" while naming only files outside
//! the change does not block. Text that cannot be read at all is an error, never a block: an
//! unreadable review is an absent review, and calling it a rejection would let a malformed reply
//! veto work nobody judged.
//!
//! Pure data and pure functions, like the rest of this crate: running the reviewer node, reading
//! the diff and re-prompting the author belong to the supervisor.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How severe a finding is. Ordered so that `Critical > High > Medium > Low`.
///
/// The variants are declared lowest first so the derived `Ord` is the severity order; the wire
/// spelling is the lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// The reviewer's spelling, case-insensitively. Only the four names are accepted: a reviewer
    /// that writes "severe" or "blocker" has not used the schema, and guessing a level for it could
    /// turn a remark into a block or a block into a remark.
    pub fn from_reviewer(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" => Some(Severity::Critical),
            "high" => Some(Severity::High),
            "medium" => Some(Severity::Medium),
            "low" => Some(Severity::Low),
            _ => None,
        }
    }
}

/// The most rounds a review may run: the first review plus at most two re-reviews after the
/// author was re-prompted. A ceiling rather than a default so a typo cannot buy an unbounded loop.
pub const MAX_ROUNDS_CEILING: u8 = 3;

/// The review policy an agent type carries.
///
/// Read from configuration through [`RawReviewSpec`], so every bound is checked once, at the edge:
/// `max_rounds` must be `1..=3` and `timeout_secs` non-zero. Out-of-range values are refused, not
/// clamped, because a clamp would run a policy the operator did not write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawReviewSpec", into = "RawReviewSpec")]
pub struct ReviewSpec {
    /// The agent type that reviews; `None` lets the supervisor pick one from a different model
    /// family than the author's (see `model_family`).
    pub agent_type: Option<String>,
    /// The least severe grounded finding that blocks.
    pub block_on: Severity,
    /// Rounds including the first; `1..=MAX_ROUNDS_CEILING`.
    pub max_rounds: u8,
    /// Wall-clock bound on one review round.
    pub timeout_secs: u64,
    /// Extra instruction for the reviewer ("security", "the migration"), appended to its prompt.
    pub focus: Option<String>,
}

impl Default for ReviewSpec {
    fn default() -> Self {
        ReviewSpec {
            agent_type: None,
            block_on: Severity::High,
            max_rounds: 1,
            timeout_secs: 600,
            focus: None,
        }
    }
}

/// [`ReviewSpec`] as written: every key optional, unknown keys refused so a misspelt one is an
/// error rather than a silently ignored policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawReviewSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_on: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReviewSpecError {
    #[error("review max_rounds must be between 1 and {MAX_ROUNDS_CEILING}, got {0}")]
    MaxRounds(u8),
    #[error("review timeout_secs must be greater than 0")]
    ZeroTimeout,
}

impl TryFrom<RawReviewSpec> for ReviewSpec {
    type Error = ReviewSpecError;

    fn try_from(raw: RawReviewSpec) -> Result<Self, Self::Error> {
        let d = ReviewSpec::default();
        let max_rounds = raw.max_rounds.unwrap_or(d.max_rounds);
        if !(1..=MAX_ROUNDS_CEILING).contains(&max_rounds) {
            return Err(ReviewSpecError::MaxRounds(max_rounds));
        }
        let timeout_secs = raw.timeout_secs.unwrap_or(d.timeout_secs);
        if timeout_secs == 0 {
            return Err(ReviewSpecError::ZeroTimeout);
        }
        Ok(ReviewSpec {
            agent_type: raw.agent_type,
            block_on: raw.block_on.unwrap_or(d.block_on),
            max_rounds,
            timeout_secs,
            focus: raw.focus,
        })
    }
}

impl From<ReviewSpec> for RawReviewSpec {
    fn from(s: ReviewSpec) -> Self {
        RawReviewSpec {
            agent_type: s.agent_type,
            block_on: Some(s.block_on),
            max_rounds: Some(s.max_rounds),
            timeout_secs: Some(s.timeout_secs),
            focus: s.focus,
        }
    }
}

/// What the reviewer itself concluded. Recorded, never obeyed: see [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelVerdict {
    Allow,
    Block,
}

/// Per-string byte caps on reviewer text. A reviewer is a foreign agent, so every string it
/// writes is bounded where it enters, like a contract's narrative.
pub const SUMMARY_CAP: usize = 1024;
pub const CLAIM_CAP: usize = 512;
pub const EVIDENCE_CAP: usize = 1024;
pub const RECOMMENDATION_CAP: usize = 512;
pub const FILE_CAP: usize = 512;

/// One finding, as recorded. `grounded` is marion's, not the reviewer's: true iff `file` is one of
/// the paths the reviewed change touched (set by [`decide`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub claim: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub recommendation: String,
    #[serde(default)]
    pub grounded: bool,
}

/// A reviewer's report: its own verdict, a summary, and its findings.
///
/// `findings_omitted` counts findings dropped by [`MAX_FINDINGS`]; absent on the wire when zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Findings {
    pub verdict: ModelVerdict,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub findings_omitted: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Which of the three readings produced a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseRoute {
    /// The whole reply was the JSON report.
    Json,
    /// The first ```json (or bare ```) fenced block was the JSON report.
    Fenced,
    /// No JSON; the first non-empty line began `ALLOW:` or `BLOCK:`.
    Line,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub findings: Findings,
    pub route: ParseRoute,
}

/// A reply none of the three readings could read. An error, **not** a block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unreadable review: {0}")]
pub struct Unparseable(pub String);

/// The report exactly as a reviewer writes it, before validation.
#[derive(Deserialize)]
struct WireReport {
    verdict: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    findings: Vec<WireFinding>,
}

#[derive(Deserialize)]
struct WireFinding {
    severity: String,
    file: String,
    #[serde(default)]
    line: Option<u32>,
    claim: String,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    recommendation: String,
}

/// Largest whole-character prefix of `s` within `max` bytes, marked with `…` when shortened.
fn cap_text(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &s[..i])
}

fn read_report(json: &str) -> Result<Findings, String> {
    let wire: WireReport = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let verdict = match wire.verdict.trim().to_ascii_lowercase().as_str() {
        "allow" => ModelVerdict::Allow,
        "block" => ModelVerdict::Block,
        other => return Err(format!("verdict {other:?} is neither allow nor block")),
    };
    let mut findings = Vec::with_capacity(wire.findings.len());
    for f in wire.findings {
        let severity = Severity::from_reviewer(&f.severity)
            .ok_or_else(|| format!("severity {:?} is not critical/high/medium/low", f.severity))?;
        findings.push(Finding {
            severity,
            file: cap_text(f.file.trim(), FILE_CAP),
            line: f.line,
            claim: cap_text(&f.claim, CLAIM_CAP),
            evidence: cap_text(&f.evidence, EVIDENCE_CAP),
            recommendation: cap_text(&f.recommendation, RECOMMENDATION_CAP),
            grounded: false,
        });
    }
    Ok(Findings {
        verdict,
        summary: cap_text(&wire.summary, SUMMARY_CAP),
        findings,
        findings_omitted: 0,
    })
}

/// The body of the first fenced block tagged `json` or untagged, if the reply has one.
fn first_json_fence(text: &str) -> Option<String> {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(info) = line.trim_start().strip_prefix("```") else {
            continue;
        };
        let info = info.trim();
        if !(info.is_empty() || info.eq_ignore_ascii_case("json")) {
            // A fence in another language: skip its body so its closing ``` is not read as an
            // opening one.
            for inner in lines.by_ref() {
                if inner.trim_start().starts_with("```") {
                    break;
                }
            }
            continue;
        }
        let mut body = String::new();
        for inner in lines.by_ref() {
            if inner.trim_start().starts_with("```") {
                return Some(body);
            }
            body.push_str(inner);
            body.push('\n');
        }
        // An unclosed fence is not a block.
        return None;
    }
    None
}

/// Read a reviewer's reply: the whole reply as the JSON report, else the first ```json (or bare
/// ```) fenced block as the JSON report, else a first non-empty line beginning `ALLOW:` or
/// `BLOCK:` (the rest of the line is the summary). Anything else is [`Unparseable`].
///
/// Strings are capped as they are read. Nothing is decided here; see [`decide`].
pub fn parse(narrative: &str) -> Result<Parsed, Unparseable> {
    let trimmed = narrative.trim();
    let strict = read_report(trimmed);
    if let Ok(findings) = strict {
        return Ok(Parsed {
            findings,
            route: ParseRoute::Json,
        });
    }
    let fenced = first_json_fence(narrative).map(|body| read_report(body.trim()));
    if let Some(Ok(findings)) = fenced {
        return Ok(Parsed {
            findings,
            route: ParseRoute::Fenced,
        });
    }
    if let Some(first) = trimmed.lines().map(str::trim).find(|l| !l.is_empty()) {
        let line = |verdict, rest: &str| Parsed {
            findings: Findings {
                verdict,
                summary: cap_text(rest.trim(), SUMMARY_CAP),
                findings: Vec::new(),
                findings_omitted: 0,
            },
            route: ParseRoute::Line,
        };
        if let Some(rest) = first.strip_prefix("ALLOW:") {
            return Ok(line(ModelVerdict::Allow, rest));
        }
        if let Some(rest) = first.strip_prefix("BLOCK:") {
            return Ok(line(ModelVerdict::Block, rest));
        }
    }
    // Name the most specific failure: a fenced block that did not validate, else the reply as JSON
    // when it looked like JSON, else that nothing matched.
    let reason = match (fenced, strict) {
        (Some(Err(e)), _) => format!("fenced JSON block is not a review report: {e}"),
        (_, Err(e)) if trimmed.starts_with('{') => format!("reply is not a review report: {e}"),
        _ if trimmed.is_empty() => "the reviewer replied with nothing".to_string(),
        _ => "no JSON report, fenced JSON block, or ALLOW:/BLOCK: first line".to_string(),
    };
    Err(Unparseable(reason))
}

/// The most findings a verdict keeps. The rest are counted in `Findings::findings_omitted`; the
/// kept ones are the grounded, most severe first, so the cap drops remarks before blockers.
pub const MAX_FINDINGS: usize = 16;

/// marion's decision on one review round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Block,
}

/// One round's outcome: marion's decision, how many grounded findings met the threshold, and the
/// report with every finding's `grounded` set and the list capped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub decision: Decision,
    pub blocking: usize,
    pub findings: Findings,
}

/// Whether a reviewer's `file` names one of the change's paths. Only a leading `./` is forgiven;
/// anything looser (basename matches, suffix matches) could ground a finding about a file the
/// change never touched.
fn grounded(file: &str, changed_paths: &[PathBuf]) -> bool {
    let mut f = file.trim();
    while let Some(rest) = f.strip_prefix("./") {
        f = rest;
    }
    !f.is_empty() && changed_paths.iter().any(|p| p.as_path() == Path::new(f))
}

/// Decide a round. **Block iff** a grounded finding (its file is in `changed_paths`) is at least
/// `block_on`, or the reviewer said block and reported no findings at all. The reviewer's own
/// verdict is otherwise only recorded: a "block" resting on ungrounded or minor findings allows,
/// and an "allow" beside a grounded blocker blocks.
///
/// Blockers are counted over the full list before the [`MAX_FINDINGS`] cap, so the cap can never
/// hide one.
pub fn decide(parsed: Parsed, changed_paths: &[PathBuf], block_on: Severity) -> Verdict {
    let mut report = parsed.findings;
    for f in report.findings.iter_mut() {
        f.grounded = grounded(&f.file, changed_paths);
    }
    let blocking = report
        .findings
        .iter()
        .filter(|f| f.grounded && f.severity >= block_on)
        .count();
    let bare_block = report.verdict == ModelVerdict::Block
        && report.findings.is_empty()
        && report.findings_omitted == 0;
    let decision = if blocking > 0 || bare_block {
        Decision::Block
    } else {
        Decision::Allow
    };
    report
        .findings
        .sort_by_key(|f| (std::cmp::Reverse(f.grounded), std::cmp::Reverse(f.severity)));
    if report.findings.len() > MAX_FINDINGS {
        report.findings_omitted += report.findings.len() - MAX_FINDINGS;
        report.findings.truncate(MAX_FINDINGS);
    }
    Verdict {
        decision,
        blocking,
        findings: report,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(toml_text: &str) -> Result<ReviewSpec, String> {
        toml::from_str::<ReviewSpec>(toml_text).map_err(|e| e.to_string())
    }

    #[test]
    fn severities_order_critical_above_high_above_medium_above_low() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
    }

    #[test]
    fn a_severity_spells_lowercase_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            r#""critical""#
        );
        assert_eq!(
            serde_json::from_str::<Severity>(r#""medium""#).unwrap(),
            Severity::Medium
        );
    }

    #[test]
    fn a_reviewers_severity_is_read_case_insensitively_and_only_by_its_four_names() {
        assert_eq!(Severity::from_reviewer(" HIGH "), Some(Severity::High));
        assert_eq!(
            Severity::from_reviewer("Critical"),
            Some(Severity::Critical)
        );
        assert_eq!(Severity::from_reviewer("low"), Some(Severity::Low));
        for guess in ["severe", "blocker", "info", "", "hi"] {
            assert_eq!(Severity::from_reviewer(guess), None, "{guess:?}");
        }
    }

    #[test]
    fn an_empty_review_table_is_the_documented_default() {
        let s = spec("").unwrap();
        assert_eq!(s, ReviewSpec::default());
        assert_eq!(s.block_on, Severity::High);
        assert_eq!(s.max_rounds, 1);
        assert_eq!(s.timeout_secs, 600);
        assert_eq!(s.agent_type, None);
        assert_eq!(s.focus, None);
    }

    #[test]
    fn every_key_is_read_when_written() {
        let s = spec(
            r#"
            agent_type = "codex-reviewer"
            block_on = "medium"
            max_rounds = 3
            timeout_secs = 90
            focus = "the migration"
            "#,
        )
        .unwrap();
        assert_eq!(
            s,
            ReviewSpec {
                agent_type: Some("codex-reviewer".into()),
                block_on: Severity::Medium,
                max_rounds: 3,
                timeout_secs: 90,
                focus: Some("the migration".into()),
            }
        );
    }

    #[test]
    fn max_rounds_outside_one_to_the_ceiling_is_refused_not_clamped() {
        assert!(spec("max_rounds = 1").is_ok());
        assert!(spec("max_rounds = 3").is_ok());
        let four = spec("max_rounds = 4").unwrap_err();
        assert!(four.contains("between 1 and 3, got 4"), "{four}");
        let zero = spec("max_rounds = 0").unwrap_err();
        assert!(zero.contains("got 0"), "{zero}");
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let e = spec("timeout_secs = 0").unwrap_err();
        assert!(e.contains("timeout_secs must be greater than 0"), "{e}");
    }

    #[test]
    fn a_misspelt_key_or_unknown_severity_is_refused() {
        assert!(spec("block_one = \"high\"").is_err());
        assert!(spec("block_on = \"severe\"").is_err());
    }

    #[test]
    fn a_spec_round_trips_through_json() {
        let s = ReviewSpec {
            agent_type: Some("r".into()),
            block_on: Severity::Critical,
            max_rounds: 2,
            timeout_secs: 30,
            focus: None,
        };
        let back: ReviewSpec = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    const REPORT: &str = r#"{"verdict":"block","summary":"one bug","findings":[
        {"severity":"high","file":"src/a.rs","line":12,"claim":"off by one",
         "evidence":"loop runs to len","recommendation":"use <"}]}"#;

    fn finding(sev: Severity, file: &str) -> Finding {
        Finding {
            severity: sev,
            file: file.into(),
            line: None,
            claim: "c".into(),
            evidence: String::new(),
            recommendation: String::new(),
            grounded: false,
        }
    }

    #[test]
    fn a_reply_that_is_the_json_report_is_read_strictly() {
        let p = parse(&format!("  \n{REPORT}\n ")).unwrap();
        assert_eq!(p.route, ParseRoute::Json);
        assert_eq!(p.findings.verdict, ModelVerdict::Block);
        assert_eq!(p.findings.summary, "one bug");
        assert_eq!(
            p.findings.findings,
            vec![Finding {
                severity: Severity::High,
                file: "src/a.rs".into(),
                line: Some(12),
                claim: "off by one".into(),
                evidence: "loop runs to len".into(),
                recommendation: "use <".into(),
                grounded: false,
            }]
        );
    }

    #[test]
    fn the_first_json_fenced_block_is_read_when_the_reply_is_prose() {
        let reply = format!("I looked.\n\n```json\n{REPORT}\n```\n\nThanks.");
        let p = parse(&reply).unwrap();
        assert_eq!(p.route, ParseRoute::Fenced);
        assert_eq!(p.findings.findings.len(), 1);
        // An untagged fence counts too.
        let bare = format!("```\n{REPORT}\n```");
        assert_eq!(parse(&bare).unwrap().route, ParseRoute::Fenced);
    }

    #[test]
    fn a_fence_in_another_language_is_skipped_whole() {
        let reply =
            "```rust\nfn main() {}\n```\n```json\n{\"verdict\":\"allow\",\"summary\":\"ok\"}\n```";
        let p = parse(reply).unwrap();
        assert_eq!(p.route, ParseRoute::Fenced);
        assert_eq!(p.findings.verdict, ModelVerdict::Allow);
    }

    #[test]
    fn a_first_line_verdict_is_read_when_there_is_no_json() {
        let a = parse("\n  ALLOW: looks fine\nmore text").unwrap();
        assert_eq!(a.route, ParseRoute::Line);
        assert_eq!(a.findings.verdict, ModelVerdict::Allow);
        assert_eq!(a.findings.summary, "looks fine");
        assert!(a.findings.findings.is_empty());
        let b = parse("BLOCK: tests delete prod data").unwrap();
        assert_eq!(b.findings.verdict, ModelVerdict::Block);
        assert_eq!(b.findings.summary, "tests delete prod data");
    }

    #[test]
    fn a_verdict_word_anywhere_but_the_first_line_or_in_lower_case_is_not_a_verdict() {
        for reply in [
            "Summary first.\nBLOCK: late",
            "block: lower case",
            "Allow: me to explain",
        ] {
            assert!(parse(reply).is_err(), "{reply:?}");
        }
    }

    #[test]
    fn an_unreadable_reply_is_an_error_naming_why_never_a_block() {
        let e = parse("I think it is mostly fine?").unwrap_err();
        assert!(e.0.contains("no JSON report"), "{e}");
        let e = parse("   ").unwrap_err();
        assert!(e.0.contains("replied with nothing"), "{e}");
        let e = parse(r#"{"verdict":"maybe"}"#).unwrap_err();
        assert!(e.0.contains("neither allow nor block"), "{e}");
        let e = parse("```json\n{\"verdict\":\"allow\",\"findings\":[{\"severity\":\"severe\",\"file\":\"a\",\"claim\":\"c\"}]}\n```").unwrap_err();
        assert!(e.0.contains("fenced JSON block"), "{e}");
        assert!(e.0.contains("severe"), "{e}");
    }

    #[test]
    fn an_unclosed_fence_is_not_a_block() {
        let reply = format!("```json\n{REPORT}");
        assert!(parse(&reply).is_err());
    }

    #[test]
    fn a_fenced_block_that_does_not_validate_falls_through_to_the_first_line() {
        let p = parse("BLOCK: see below\n```json\nnot json\n```").unwrap();
        assert_eq!(p.route, ParseRoute::Line);
        assert_eq!(p.findings.verdict, ModelVerdict::Block);
    }

    #[test]
    fn verdict_and_severity_spellings_are_case_insensitive_and_optional_keys_default() {
        let p = parse(
            r#"{"verdict":"ALLOW","findings":[{"severity":"Low","file":" ./x.rs ","claim":"nit"}]}"#,
        )
        .unwrap();
        assert_eq!(p.findings.verdict, ModelVerdict::Allow);
        assert_eq!(p.findings.summary, "");
        let f = &p.findings.findings[0];
        assert_eq!(f.severity, Severity::Low);
        assert_eq!(f.file, "./x.rs");
        assert_eq!((f.line, f.evidence.as_str()), (None, ""));
    }

    #[test]
    fn a_duplicated_key_in_a_report_is_refused() {
        assert!(parse(r#"{"verdict":"allow","verdict":"block"}"#).is_err());
    }

    #[test]
    fn every_reviewer_string_is_capped_on_a_character_boundary() {
        let long = "é".repeat(4000);
        let json = serde_json::json!({
            "verdict": "block",
            "summary": long,
            "findings": [{"severity":"high","file":long,"claim":long,"evidence":long,"recommendation":long}]
        })
        .to_string();
        let p = parse(&json).unwrap();
        let f = &p.findings.findings[0];
        for (s, cap) in [
            (&p.findings.summary, SUMMARY_CAP),
            (&f.file, FILE_CAP),
            (&f.claim, CLAIM_CAP),
            (&f.evidence, EVIDENCE_CAP),
            (&f.recommendation, RECOMMENDATION_CAP),
        ] {
            assert!(s.ends_with('…'), "marked as shortened");
            assert!(s.len() <= cap + '…'.len_utf8(), "{} > {cap}", s.len());
        }
        assert_eq!(parse("ALLOW: short").unwrap().findings.summary, "short");
    }

    #[test]
    fn a_report_round_trips_and_omits_a_zero_omitted_count() {
        let r = Findings {
            verdict: ModelVerdict::Block,
            summary: "s".into(),
            findings: vec![finding(Severity::Critical, "a.rs")],
            findings_omitted: 0,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("findings_omitted"), "{json}");
        assert!(!json.contains("\"line\""), "{json}");
        assert_eq!(serde_json::from_str::<Findings>(&json).unwrap(), r);
        let omitted = Findings {
            findings_omitted: 3,
            ..r
        };
        let json = serde_json::to_string(&omitted).unwrap();
        assert!(json.contains(r#""findings_omitted":3"#), "{json}");
        assert_eq!(serde_json::from_str::<Findings>(&json).unwrap(), omitted);
    }

    fn report(verdict: ModelVerdict, findings: Vec<Finding>) -> Parsed {
        Parsed {
            findings: Findings {
                verdict,
                summary: String::new(),
                findings,
                findings_omitted: 0,
            },
            route: ParseRoute::Json,
        }
    }

    fn changed() -> Vec<PathBuf> {
        vec![PathBuf::from("src/a.rs"), PathBuf::from("docs/b.md")]
    }

    #[test]
    fn a_grounded_finding_at_the_threshold_blocks_and_one_below_does_not() {
        for (sev, want) in [
            (Severity::Critical, Decision::Block),
            (Severity::High, Decision::Block),
            (Severity::Medium, Decision::Allow),
            (Severity::Low, Decision::Allow),
        ] {
            let v = decide(
                report(ModelVerdict::Block, vec![finding(sev, "src/a.rs")]),
                &changed(),
                Severity::High,
            );
            assert_eq!(v.decision, want, "{sev:?}");
            assert_eq!(v.blocking, usize::from(want == Decision::Block));
        }
    }

    #[test]
    fn the_threshold_edges_low_blocks_on_anything_and_critical_only_on_critical() {
        let low = report(
            ModelVerdict::Allow,
            vec![finding(Severity::Low, "src/a.rs")],
        );
        assert_eq!(
            decide(low, &changed(), Severity::Low).decision,
            Decision::Block
        );
        let high = report(
            ModelVerdict::Block,
            vec![finding(Severity::High, "src/a.rs")],
        );
        assert_eq!(
            decide(high, &changed(), Severity::Critical).decision,
            Decision::Allow
        );
        let crit = report(
            ModelVerdict::Block,
            vec![finding(Severity::Critical, "src/a.rs")],
        );
        assert_eq!(
            decide(crit, &changed(), Severity::Critical).decision,
            Decision::Block
        );
    }

    #[test]
    fn an_ungrounded_finding_is_recorded_as_such_and_never_blocks() {
        let v = decide(
            report(
                ModelVerdict::Block,
                vec![finding(Severity::Critical, "src/elsewhere.rs")],
            ),
            &changed(),
            Severity::Low,
        );
        assert_eq!(v.decision, Decision::Allow);
        assert_eq!(v.blocking, 0);
        assert!(!v.findings.findings[0].grounded);
        // The reviewer's own verdict is kept on the record even though marion overrode it.
        assert_eq!(v.findings.verdict, ModelVerdict::Block);
    }

    #[test]
    fn grounding_forgives_a_leading_dot_slash_and_nothing_looser() {
        let cases = [
            ("./src/a.rs", true),
            ("././docs/b.md", true),
            (" src/a.rs ", true),
            ("a.rs", false),
            ("/abs/src/a.rs", false),
            ("src/a.rs.orig", false),
            ("", false),
        ];
        for (file, want) in cases {
            let v = decide(
                report(ModelVerdict::Allow, vec![finding(Severity::High, file)]),
                &changed(),
                Severity::High,
            );
            assert_eq!(v.findings.findings[0].grounded, want, "{file:?}");
        }
    }

    #[test]
    fn a_block_with_no_findings_blocks_and_an_allow_with_none_allows() {
        let v = decide(
            report(ModelVerdict::Block, vec![]),
            &changed(),
            Severity::High,
        );
        assert_eq!((v.decision, v.blocking), (Decision::Block, 0));
        let v = decide(
            report(ModelVerdict::Allow, vec![]),
            &changed(),
            Severity::High,
        );
        assert_eq!(v.decision, Decision::Allow);
        // The first-line form reaches the same rule.
        let line = parse("BLOCK: drops the users table").unwrap();
        assert_eq!(
            decide(line, &changed(), Severity::High).decision,
            Decision::Block
        );
    }

    #[test]
    fn a_block_resting_only_on_minor_findings_allows() {
        let v = decide(
            report(
                ModelVerdict::Block,
                vec![finding(Severity::Low, "src/a.rs")],
            ),
            &changed(),
            Severity::High,
        );
        assert_eq!(v.decision, Decision::Allow);
    }

    #[test]
    fn an_allow_beside_a_grounded_blocker_blocks() {
        let v = decide(
            report(
                ModelVerdict::Allow,
                vec![finding(Severity::Critical, "docs/b.md")],
            ),
            &changed(),
            Severity::High,
        );
        assert_eq!(v.decision, Decision::Block);
    }

    #[test]
    fn the_cap_keeps_grounded_and_severe_findings_and_counts_what_it_drops() {
        let mut fs: Vec<Finding> = (0..MAX_FINDINGS + 4)
            .map(|_| finding(Severity::Critical, "not/changed.rs"))
            .collect();
        // The one grounded blocker comes last and is below every ungrounded one in severity.
        fs.push(finding(Severity::High, "src/a.rs"));
        let n = fs.len();
        let v = decide(report(ModelVerdict::Allow, fs), &changed(), Severity::High);
        assert_eq!(v.decision, Decision::Block);
        assert_eq!(v.blocking, 1);
        assert_eq!(v.findings.findings.len(), MAX_FINDINGS);
        assert_eq!(v.findings.findings_omitted, n - MAX_FINDINGS);
        assert!(v.findings.findings[0].grounded, "grounded first");
        assert_eq!(v.findings.findings[0].file, "src/a.rs");
    }

    #[test]
    fn within_the_cap_findings_are_ordered_grounded_then_by_severity() {
        let v = decide(
            report(
                ModelVerdict::Allow,
                vec![
                    finding(Severity::Low, "src/a.rs"),
                    finding(Severity::Critical, "x.rs"),
                    finding(Severity::Medium, "src/a.rs"),
                ],
            ),
            &changed(),
            Severity::Critical,
        );
        let order: Vec<_> = v
            .findings
            .findings
            .iter()
            .map(|f| (f.grounded, f.severity))
            .collect();
        assert_eq!(
            order,
            vec![
                (true, Severity::Medium),
                (true, Severity::Low),
                (false, Severity::Critical)
            ]
        );
        assert_eq!(v.findings.findings_omitted, 0);
    }
}
