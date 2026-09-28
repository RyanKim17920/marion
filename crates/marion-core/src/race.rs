//! Race: one task run once per candidate `agent_type[:model]`, each seat in its own worktree under
//! the same verification, and a winner picked by marion.
//!
//! **marion decides, never the model.** A seat wins by the evidence marion collected — its exit
//! status, its verification lines, the tokens its harness reported and the time it took — ranked by
//! the stages of a [`RacePolicy`]. Nothing a seat writes about itself is read here.
//!
//! Pure data and pure functions, like the rest of this crate: [`step`] reads a [`RaceState`] and
//! says what to do next; launching seats, stopping them and writing the result belong to the
//! supervisor. Because `step` reads only state, calling it again after any change is always safe,
//! which is what lets a restarted supervisor re-drive a race it found open in the journal.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::agent_type::{is_command_type, is_valid_name};
use crate::contract::{AgentId, ExitStatus, TaskId};
use crate::harness::Harness;
use crate::ids::{RAND_BYTES, uuid_v7};

/// The fewest seats a race has: one seat is a plain spawn.
pub const MIN_SEATS: usize = 2;
/// The most seats a race has. A race costs its seat count in tokens, and eight keeps the
/// [`crate::journal`] record naming them far under its cap.
pub const MAX_SEATS: usize = 8;
/// The longest candidate string, in bytes. Seat specs are journaled, so they are bounded where
/// they enter.
pub const CANDIDATE_CAP: usize = 128;

/// A race's id: a UUIDv7, like every other id marion mints, and a filesystem path component
/// (`races/<race_id>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RaceId(pub String);

/// Mint a [`RaceId`] from the caller's clock reading and entropy; this crate performs no I/O.
pub fn new_race_id(unix_millis: u64, rand: [u8; RAND_BYTES]) -> RaceId {
    RaceId(uuid_v7(unix_millis, rand))
}

/// Which race a node belongs to and why it is there, recorded on its spawn intent so a resumed
/// seat keeps its seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaceSeat {
    pub race_id: RaceId,
    pub role: RaceRole,
}

/// A node's part in a race. Externally tagged (`{"Candidate":2}`), so a later role is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RaceRole {
    /// Runs the task; 1-based seat number.
    Candidate(u8),
}

impl RaceSeat {
    /// The candidate seat, where this node runs the task.
    pub fn seat(&self) -> Option<u8> {
        match self.role {
            RaceRole::Candidate(n) => Some(n),
        }
    }
}

/// One seat as asked for: an agent type and, optionally, the model it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Candidate {
    /// `type[:model]`, split on the **first** colon, so `opencode:openrouter:qwen` is type
    /// `opencode` running model `openrouter:qwen` (which the provider table splits again later).
    ///
    /// The one exception is a whole-command type ([`is_command_type`], `acp:<command>`), whose name
    /// is itself a colon spelling: such a candidate is taken whole and names no model, because a
    /// command may contain colons and any split of it would be a guess.
    pub fn parse(s: &str) -> Result<Candidate, RaceError> {
        let bad = || RaceError::BadCandidate(s.chars().take(CANDIDATE_CAP).collect());
        if s.len() > CANDIDATE_CAP {
            return Err(RaceError::CandidateTooLong(s.len()));
        }
        if s.chars().any(char::is_control) {
            return Err(bad());
        }
        if is_command_type(s) {
            return Ok(Candidate {
                agent_type: s.to_string(),
                model: None,
            });
        }
        let (agent_type, model) = match s.split_once(':') {
            Some((t, m)) => (t, Some(m)),
            None => (s, None),
        };
        if !is_valid_name(agent_type) {
            return Err(bad());
        }
        if let Some(m) = model
            && (m.is_empty() || m.chars().any(char::is_whitespace))
        {
            return Err(bad());
        }
        Ok(Candidate {
            agent_type: agent_type.to_string(),
            model: model.map(str::to_string),
        })
    }

    /// The spelling [`Self::parse`] reads back: `type` or `type:model`.
    pub fn label(&self) -> String {
        match &self.model {
            Some(m) => format!("{}:{m}", self.agent_type),
            None => self.agent_type.clone(),
        }
    }
}

/// A request's candidate list, parsed whole: `MIN_SEATS..=MAX_SEATS` of them, each well formed.
/// The same candidate twice is allowed — two runs of one model is a fair race when the model is
/// not deterministic.
pub fn parse_candidates(raw: &[String]) -> Result<Vec<Candidate>, RaceError> {
    if raw.len() < MIN_SEATS {
        return Err(RaceError::TooFewSeats(raw.len()));
    }
    if raw.len() > MAX_SEATS {
        return Err(RaceError::TooManySeats(raw.len()));
    }
    raw.iter().map(|s| Candidate::parse(s)).collect()
}

/// One ranking stage. Verification is a filter, the others order the survivors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Keep only seats that exited `Ok` with every verification line passing.
    Verify,
    /// Fewest tokens first; a seat whose harness reported none sorts last.
    Tokens,
    /// Shortest run first; a seat with no measured time sorts last.
    Time,
}

/// What happens to the losers' branches once a winner is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Losers {
    /// Every seat's branch stays, for the operator to compare.
    #[default]
    Keep,
    /// A loser's own `marion/<task_id>` branch and worktree are removed. Never when nobody won.
    Prune,
}

/// How a race is judged. Read through [`RawRacePolicy`], so every rule is checked once, at the
/// edge: stages are non-empty, start with [`Stage::Verify`] and name no stage twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRacePolicy", into = "RawRacePolicy")]
pub struct RacePolicy {
    pub stages: Vec<Stage>,
    /// The first seat to pass verification wins and the rest are stopped: time over tokens.
    pub first: bool,
    pub losers: Losers,
}

impl Default for RacePolicy {
    fn default() -> Self {
        RacePolicy {
            stages: vec![Stage::Verify, Stage::Tokens, Stage::Time],
            first: false,
            losers: Losers::Keep,
        }
    }
}

/// [`RacePolicy`] as written — the `[race]` table of `.marion/agents.toml`, or a spawn's `race`
/// object. Every key optional, unknown keys refused so a misspelt one is an error rather than a
/// silently ignored rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRacePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stages: Option<Vec<Stage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub losers: Option<Losers>,
}

impl RawRacePolicy {
    /// This policy's keys over `base`'s: a spawn's `race` object over the tree's `[race]` table,
    /// key by key, so a request that says only `first` keeps the operator's stages.
    pub fn over(self, base: RawRacePolicy) -> RawRacePolicy {
        RawRacePolicy {
            stages: self.stages.or(base.stages),
            first: self.first.or(base.first),
            losers: self.losers.or(base.losers),
        }
    }
}

impl TryFrom<RawRacePolicy> for RacePolicy {
    type Error = RaceError;

    fn try_from(raw: RawRacePolicy) -> Result<Self, Self::Error> {
        let d = RacePolicy::default();
        let stages = raw.stages.unwrap_or(d.stages);
        if stages.first() != Some(&Stage::Verify) {
            return Err(RaceError::VerifyFirst);
        }
        for (i, s) in stages.iter().enumerate() {
            if stages[..i].contains(s) {
                return Err(RaceError::DuplicateStage(*s));
            }
        }
        Ok(RacePolicy {
            stages,
            first: raw.first.unwrap_or(d.first),
            losers: raw.losers.unwrap_or(d.losers),
        })
    }
}

impl From<RacePolicy> for RawRacePolicy {
    fn from(p: RacePolicy) -> Self {
        RawRacePolicy {
            stages: Some(p.stages),
            first: Some(p.first),
            losers: Some(p.losers),
        }
    }
}

/// Why a race was refused. Every one is decided before anything is launched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RaceError {
    #[error("a race needs at least {MIN_SEATS} candidates, got {0}")]
    TooFewSeats(usize),
    #[error("a race takes at most {MAX_SEATS} candidates, got {0}")]
    TooManySeats(usize),
    #[error(
        "candidate {0:?} is not `agent_type[:model]` (a type name, then optionally a colon and a \
         model with no spaces)"
    )]
    BadCandidate(String),
    #[error("a candidate is at most {CANDIDATE_CAP} bytes, got {0}")]
    CandidateTooLong(usize),
    #[error("race stages must start with \"verify\"")]
    VerifyFirst,
    #[error("race stage {0:?} is named twice")]
    DuplicateStage(Stage),
    #[error(
        "a race needs verification: give `verification` commands, so marion can tell a passing \
         seat from a failing one"
    )]
    NoVerification,
}

/// Refuse a race marion could not judge. Without verification every seat that exits `Ok` passes,
/// and the ranking stages would pick the cheapest seat rather than the one that did the work.
pub fn check_race(verification_lines: usize) -> Result<(), RaceError> {
    if verification_lines == 0 {
        return Err(RaceError::NoVerification);
    }
    Ok(())
}

/// How a seat came out of a decided race.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeatVerdict {
    Won,
    /// Passed verification and was outranked.
    Lost,
    /// Did not pass: a failed, timed-out or unreported run, or a verification line that failed.
    Failed,
    /// Stopped or killed before it could finish.
    Cancelled,
    /// Never started: its harness could not be launched.
    Unlaunched,
}

/// What settled the race: the stage at which one seat was left, or why nobody won.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecidedBy {
    /// Exactly one seat passed verification.
    Verification,
    Tokens,
    Time,
    /// Every stage tied; the lowest seat wins.
    SeatOrder,
    /// `first`: the first seat to pass.
    FirstPass,
    /// Nobody passed.
    NoPass,
    /// The requester went away before a winner.
    Abandoned,
}

/// A finished seat's evidence, as marion collected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatOutcome {
    pub status: ExitStatus,
    /// Verification lines that exited 0, of how many ran.
    pub verified: (u16, u16),
    pub tokens: Option<u64>,
    pub secs: Option<u64>,
    pub branch: Option<String>,
    /// The order seats finished in, from 0. `first` reads it; nothing else does.
    pub order: u32,
}

impl SeatOutcome {
    /// Verification's filter: an `Ok` exit and every line passing.
    pub fn passed(&self) -> bool {
        self.status == ExitStatus::Ok && self.verified.1 > 0 && self.verified.0 == self.verified.1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatRun {
    Unlaunched,
    Running,
    Finished(SeatOutcome),
}

/// One seat as the supervisor knows it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatState {
    /// 1-based, the order the candidates were given in.
    pub seat: u8,
    pub candidate: Candidate,
    pub harness: Option<Harness>,
    pub agent_id: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub run: SeatRun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaceState {
    pub race_id: RaceId,
    pub requester: AgentId,
    pub seats: Vec<SeatState>,
    /// The requester is gone: no winner will be delivered to anyone.
    pub abandoned: bool,
}

/// What the supervisor should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Seats are still running.
    Wait,
    /// Stop these seats (1-based); the race is decided once they have exited.
    Stop(Vec<u8>),
    Decided(RaceResult),
}

/// One scoreboard row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreRow {
    pub seat: u8,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ExitStatus>,
    pub verified: (u16, u16),
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub verdict: SeatVerdict,
    /// Set by the supervisor once the branch is actually gone, never predicted.
    #[serde(default)]
    pub pruned: bool,
}

impl ScoreRow {
    pub fn label(&self) -> String {
        Candidate {
            agent_type: self.agent_type.clone(),
            model: self.model.clone(),
        }
        .label()
    }

    /// `#2 claude:sonnet ★ pass 3/3 41k tok 3m12s marion/t-…` — the line a parent reads.
    pub fn line(&self) -> String {
        let mut s = format!("#{} {}", self.seat, self.label());
        let word = match (self.verdict, self.status) {
            (SeatVerdict::Unlaunched, _) => {
                s.push_str(" unlaunched");
                return s;
            }
            (SeatVerdict::Won, _) => "★ pass",
            (SeatVerdict::Lost, _) => "pass",
            (SeatVerdict::Cancelled, _) => "cancelled",
            (SeatVerdict::Failed, Some(ExitStatus::Ok) | None) => "fail",
            (SeatVerdict::Failed, Some(st)) => status_word(st),
        };
        let _ = write!(s, " {word} {}/{}", self.verified.0, self.verified.1);
        if let Some(t) = self.tokens {
            let _ = write!(s, " {} tok", tokens_short(t));
        }
        if let Some(secs) = self.secs {
            let _ = write!(s, " {}", secs_short(secs));
        }
        if let Some(b) = &self.branch {
            let _ = write!(s, " {b}");
            if self.pruned {
                s.push_str(" (pruned)");
            }
        }
        s
    }
}

fn status_word(st: ExitStatus) -> &'static str {
    match st {
        ExitStatus::Ok => "ok",
        ExitStatus::Failed => "failed",
        ExitStatus::Cancelled => "cancelled",
        ExitStatus::Unreported => "unreported",
        ExitStatus::TimedOut => "timed out",
        ExitStatus::Killed => "killed",
    }
}

/// `950`, `41k`, `1.2M`.
pub fn tokens_short(t: u64) -> String {
    match t {
        0..1_000 => t.to_string(),
        1_000..1_000_000 => format!("{}k", t / 1_000),
        _ => format!("{}.{}M", t / 1_000_000, (t % 1_000_000) / 100_000),
    }
}

/// `45s`, `3m12s`, `1h02m`.
pub fn secs_short(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3_600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
    }
}

/// A decided race: the authoritative `races/<race_id>.json`. No prompt, no seat's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaceResult {
    pub race_id: RaceId,
    pub requester: AgentId,
    pub policy: RacePolicy,
    /// The winning seat, 1-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<u8>,
    pub decided_by: DecidedBy,
    pub seats: Vec<ScoreRow>,
}

impl RaceResult {
    pub fn winner_row(&self) -> Option<&ScoreRow> {
        let w = self.winner?;
        self.seats.iter().find(|r| r.seat == w)
    }

    /// Seats whose branch pruning would remove: only under [`Losers::Prune`], only when somebody
    /// won (a race nobody won keeps every branch, since one may be nearly right), and only a
    /// loser's own recorded branch.
    pub fn prunable(&self) -> Vec<u8> {
        if self.policy.losers != Losers::Prune || self.winner.is_none() {
            return Vec::new();
        }
        self.seats
            .iter()
            .filter(|r| r.verdict != SeatVerdict::Won && r.branch.is_some() && !r.pruned)
            .map(|r| r.seat)
            .collect()
    }

    /// The scoreboard, one [`ScoreRow::line`] per seat in seat order, then what decided it.
    pub fn scoreboard(&self) -> String {
        let mut s = String::new();
        for r in &self.seats {
            s.push_str(&r.line());
            s.push('\n');
        }
        let _ = write!(s, "decided by: {}", decided_word(self.decided_by));
        s
    }
}

fn decided_word(d: DecidedBy) -> &'static str {
    match d {
        DecidedBy::Verification => "verification",
        DecidedBy::Tokens => "tokens",
        DecidedBy::Time => "time",
        DecidedBy::SeatOrder => "seat order",
        DecidedBy::FirstPass => "first pass",
        DecidedBy::NoPass => "no seat passed",
        DecidedBy::Abandoned => "abandoned",
    }
}

/// The next thing to do for `state` under `policy`. Idempotent: the same state always gives the
/// same step, so the supervisor may call it after every change and after a restart.
pub fn step(policy: &RacePolicy, state: &RaceState) -> Step {
    let running: Vec<u8> = state
        .seats
        .iter()
        .filter(|s| s.run == SeatRun::Running)
        .map(|s| s.seat)
        .collect();
    let passers: Vec<(&SeatState, &SeatOutcome)> = state
        .seats
        .iter()
        .filter_map(|s| match &s.run {
            SeatRun::Finished(o) if o.passed() => Some((s, o)),
            _ => None,
        })
        .collect();

    if state.abandoned {
        if !running.is_empty() {
            return Step::Stop(running);
        }
        return Step::Decided(result(policy, state, None, DecidedBy::Abandoned));
    }

    if policy.first {
        return match passers.iter().min_by_key(|(s, o)| (o.order, s.seat)) {
            Some(_) if !running.is_empty() => Step::Stop(running),
            Some((s, _)) => {
                Step::Decided(result(policy, state, Some(s.seat), DecidedBy::FirstPass))
            }
            None if !running.is_empty() => Step::Wait,
            None => Step::Decided(result(policy, state, None, DecidedBy::NoPass)),
        };
    }

    if !running.is_empty() {
        return Step::Wait;
    }
    let (winner, by) = rank(policy, passers);
    Step::Decided(result(policy, state, winner, by))
}

/// Verification has filtered to `passers`; each later stage keeps only the seats tied for its best
/// key, and the stage that leaves one seat is what decided it.
fn rank(policy: &RacePolicy, mut left: Vec<(&SeatState, &SeatOutcome)>) -> (Option<u8>, DecidedBy) {
    match left.len() {
        0 => return (None, DecidedBy::NoPass),
        1 => return (Some(left[0].0.seat), DecidedBy::Verification),
        _ => {}
    }
    for stage in &policy.stages {
        // An absent claim sorts last: no claim is not a claim of zero.
        let key = |o: &SeatOutcome| -> u64 {
            match stage {
                Stage::Verify => 0,
                Stage::Tokens => o.tokens.unwrap_or(u64::MAX),
                Stage::Time => o.secs.unwrap_or(u64::MAX),
            }
        };
        let best = left.iter().map(|(_, o)| key(o)).min().unwrap_or(0);
        left.retain(|(_, o)| key(o) == best);
        if left.len() == 1 {
            let by = match stage {
                Stage::Verify => DecidedBy::Verification,
                Stage::Tokens => DecidedBy::Tokens,
                Stage::Time => DecidedBy::Time,
            };
            return (Some(left[0].0.seat), by);
        }
    }
    let seat = left.iter().map(|(s, _)| s.seat).min();
    (seat, DecidedBy::SeatOrder)
}

fn result(policy: &RacePolicy, state: &RaceState, winner: Option<u8>, by: DecidedBy) -> RaceResult {
    let seats = state
        .seats
        .iter()
        .map(|s| {
            let (outcome, verdict) = match &s.run {
                SeatRun::Unlaunched => (None, SeatVerdict::Unlaunched),
                // Only reachable for a seat that never reported an exit, which `step` decides
                // over solely when the race was abandoned.
                SeatRun::Running => (None, SeatVerdict::Cancelled),
                SeatRun::Finished(o) => {
                    let v = if Some(s.seat) == winner {
                        SeatVerdict::Won
                    } else if o.passed() {
                        SeatVerdict::Lost
                    } else if matches!(o.status, ExitStatus::Cancelled | ExitStatus::Killed) {
                        SeatVerdict::Cancelled
                    } else {
                        SeatVerdict::Failed
                    };
                    (Some(o), v)
                }
            };
            ScoreRow {
                seat: s.seat,
                agent_type: s.candidate.agent_type.clone(),
                model: s.candidate.model.clone(),
                harness: s.harness,
                agent_id: s.agent_id.clone(),
                task_id: s.task_id.clone(),
                status: outcome.map(|o| o.status),
                verified: outcome.map_or((0, 0), |o| o.verified),
                tokens: outcome.and_then(|o| o.tokens),
                secs: outcome.and_then(|o| o.secs),
                branch: outcome.and_then(|o| o.branch.clone()),
                verdict,
                pruned: false,
            }
        })
        .collect();
    RaceResult {
        race_id: state.race_id.clone(),
        requester: state.requester.clone(),
        policy: policy.clone(),
        winner,
        decided_by: by,
        seats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Candidate {
        Candidate::parse(s).unwrap()
    }

    fn done(
        status: ExitStatus,
        verified: (u16, u16),
        tokens: Option<u64>,
        secs: Option<u64>,
        order: u32,
    ) -> SeatRun {
        SeatRun::Finished(SeatOutcome {
            status,
            verified,
            tokens,
            secs,
            branch: Some(format!("marion/t-{order}")),
            order,
        })
    }

    fn race(runs: Vec<SeatRun>) -> RaceState {
        RaceState {
            race_id: RaceId("r".into()),
            requester: AgentId("p".into()),
            seats: runs
                .into_iter()
                .enumerate()
                .map(|(i, run)| SeatState {
                    seat: i as u8 + 1,
                    candidate: c(&format!("canned:m{}", i + 1)),
                    harness: Some(Harness::ClaudeCode),
                    agent_id: Some(AgentId(format!("a-{}", i + 1))),
                    task_id: Some(TaskId(format!("t-{}", i + 1))),
                    run,
                })
                .collect(),
            abandoned: false,
        }
    }

    fn decided(step: Step) -> RaceResult {
        match step {
            Step::Decided(r) => r,
            other => panic!("not decided: {other:?}"),
        }
    }

    fn verdicts(r: &RaceResult) -> Vec<SeatVerdict> {
        r.seats.iter().map(|s| s.verdict).collect()
    }

    const OK: ExitStatus = ExitStatus::Ok;

    #[test]
    fn a_candidate_splits_on_its_first_colon() {
        assert_eq!(
            c("claude"),
            Candidate {
                agent_type: "claude".into(),
                model: None
            }
        );
        assert_eq!(c("claude:sonnet").model.as_deref(), Some("sonnet"));
        let oc = c("opencode:openrouter:qwen");
        assert_eq!(
            (oc.agent_type.as_str(), oc.model.as_deref()),
            ("opencode", Some("openrouter:qwen"))
        );
        assert_eq!(oc.label(), "opencode:openrouter:qwen");
    }

    #[test]
    fn an_acp_candidate_is_taken_whole() {
        let a = c("acp:goose acp");
        assert_eq!((a.agent_type.as_str(), a.model), ("acp:goose acp", None));
        assert!(Candidate::parse("acp:  ").is_err());
    }

    #[test]
    fn a_malformed_candidate_is_refused() {
        for bad in [
            "",
            ":m",
            "claude:",
            "cl aude",
            "claude:a b",
            "-x",
            "claude:\u{7}",
            "a\nb",
        ] {
            assert!(
                matches!(Candidate::parse(bad), Err(RaceError::BadCandidate(_))),
                "{bad:?}"
            );
        }
        let long = format!("claude:{}", "m".repeat(CANDIDATE_CAP));
        assert_eq!(
            Candidate::parse(&long),
            Err(RaceError::CandidateTooLong(long.len()))
        );
    }

    #[test]
    fn a_race_has_two_to_eight_seats() {
        let n = |k: usize| vec!["claude".to_string(); k];
        assert_eq!(parse_candidates(&n(1)), Err(RaceError::TooFewSeats(1)));
        assert_eq!(parse_candidates(&n(9)), Err(RaceError::TooManySeats(9)));
        assert_eq!(parse_candidates(&n(2)).unwrap().len(), 2);
        assert_eq!(parse_candidates(&n(8)).unwrap().len(), 8);
    }

    #[test]
    fn a_race_without_verification_is_refused() {
        assert_eq!(check_race(0), Err(RaceError::NoVerification));
        assert_eq!(check_race(1), Ok(()));
    }

    #[test]
    fn the_policy_defaults_and_refuses_bad_stages() {
        let p = RacePolicy::try_from(RawRacePolicy::default()).unwrap();
        assert_eq!(p, RacePolicy::default());
        let raw = |stages: Vec<Stage>| RawRacePolicy {
            stages: Some(stages),
            ..Default::default()
        };
        assert_eq!(
            RacePolicy::try_from(raw(vec![])),
            Err(RaceError::VerifyFirst)
        );
        assert_eq!(
            RacePolicy::try_from(raw(vec![Stage::Tokens, Stage::Verify])),
            Err(RaceError::VerifyFirst)
        );
        assert_eq!(
            RacePolicy::try_from(raw(vec![Stage::Verify, Stage::Time, Stage::Time])),
            Err(RaceError::DuplicateStage(Stage::Time))
        );
        assert!(RacePolicy::try_from(raw(vec![Stage::Verify])).is_ok());
    }

    #[test]
    fn the_policy_reads_from_toml_and_refuses_unknown_keys() {
        let p: RacePolicy =
            toml::from_str("stages = [\"verify\", \"time\"]\nlosers = \"prune\"").unwrap();
        assert_eq!(p.stages, vec![Stage::Verify, Stage::Time]);
        assert_eq!(p.losers, Losers::Prune);
        assert!(!p.first);
        assert!(toml::from_str::<RacePolicy>("frist = true").is_err());
        assert!(toml::from_str::<RacePolicy>("stages = [\"vibes\"]").is_err());
        let back: RacePolicy = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn a_request_policy_overrides_the_tree_key_by_key() {
        let tree = RawRacePolicy {
            stages: Some(vec![Stage::Verify, Stage::Time]),
            losers: Some(Losers::Prune),
            first: None,
        };
        let req = RawRacePolicy {
            first: Some(true),
            losers: Some(Losers::Keep),
            stages: None,
        };
        let p = RacePolicy::try_from(req.over(tree)).unwrap();
        assert_eq!(
            p,
            RacePolicy {
                stages: vec![Stage::Verify, Stage::Time],
                first: true,
                losers: Losers::Keep
            }
        );
    }

    #[test]
    fn a_race_waits_while_any_seat_runs() {
        let s = race(vec![
            done(OK, (1, 1), Some(5), Some(5), 0),
            SeatRun::Running,
        ]);
        assert_eq!(step(&RacePolicy::default(), &s), Step::Wait);
    }

    #[test]
    fn the_one_passing_seat_wins_by_verification() {
        let s = race(vec![
            done(OK, (0, 1), Some(1), Some(1), 0),
            done(OK, (1, 1), Some(900), Some(900), 1),
            done(ExitStatus::Failed, (0, 1), None, Some(3), 2),
        ]);
        let r = decided(step(&RacePolicy::default(), &s));
        assert_eq!((r.winner, r.decided_by), (Some(2), DecidedBy::Verification));
        assert_eq!(
            verdicts(&r),
            vec![SeatVerdict::Failed, SeatVerdict::Won, SeatVerdict::Failed]
        );
    }

    #[test]
    fn an_ok_exit_with_no_lines_run_does_not_pass() {
        let s = race(vec![
            done(OK, (0, 0), Some(1), Some(1), 0),
            done(OK, (2, 2), Some(9), Some(9), 1),
        ]);
        assert_eq!(decided(step(&RacePolicy::default(), &s)).winner, Some(2));
    }

    #[test]
    fn passers_are_ranked_by_tokens_then_time_then_seat() {
        let p = RacePolicy::default();
        let tok = race(vec![
            done(OK, (1, 1), Some(50), Some(1), 0),
            done(OK, (1, 1), Some(40), Some(99), 1),
        ]);
        let r = decided(step(&p, &tok));
        assert_eq!((r.winner, r.decided_by), (Some(2), DecidedBy::Tokens));
        assert_eq!(verdicts(&r), vec![SeatVerdict::Lost, SeatVerdict::Won]);

        let time = race(vec![
            done(OK, (1, 1), Some(40), Some(9), 0),
            done(OK, (1, 1), Some(40), Some(8), 1),
        ]);
        assert_eq!(decided(step(&p, &time)).decided_by, DecidedBy::Time);
        assert_eq!(decided(step(&p, &time)).winner, Some(2));

        let tie = race(vec![
            done(ExitStatus::Failed, (0, 1), None, None, 0),
            done(OK, (1, 1), Some(40), Some(8), 1),
            done(OK, (1, 1), Some(40), Some(8), 2),
        ]);
        let r = decided(step(&p, &tie));
        assert_eq!((r.winner, r.decided_by), (Some(2), DecidedBy::SeatOrder));
    }

    #[test]
    fn a_seat_with_no_token_claim_sorts_last() {
        let s = race(vec![
            done(OK, (1, 1), None, Some(1), 0),
            done(OK, (1, 1), Some(10_000_000), Some(9), 1),
        ]);
        assert_eq!(decided(step(&RacePolicy::default(), &s)).winner, Some(2));
    }

    #[test]
    fn the_stage_order_is_the_policys() {
        let p = RacePolicy {
            stages: vec![Stage::Verify, Stage::Time, Stage::Tokens],
            ..Default::default()
        };
        let s = race(vec![
            done(OK, (1, 1), Some(50), Some(1), 0),
            done(OK, (1, 1), Some(40), Some(99), 1),
        ]);
        let r = decided(step(&p, &s));
        assert_eq!((r.winner, r.decided_by), (Some(1), DecidedBy::Time));
    }

    #[test]
    fn nobody_passing_is_no_pass_and_keeps_every_branch() {
        let p = RacePolicy {
            losers: Losers::Prune,
            ..Default::default()
        };
        let s = race(vec![
            done(ExitStatus::TimedOut, (0, 1), None, None, 0),
            done(OK, (0, 2), Some(1), Some(1), 1),
        ]);
        let r = decided(step(&p, &s));
        assert_eq!((r.winner, r.decided_by), (None, DecidedBy::NoPass));
        assert_eq!(verdicts(&r), vec![SeatVerdict::Failed, SeatVerdict::Failed]);
        assert!(r.prunable().is_empty());
    }

    #[test]
    fn prune_names_only_losers_with_a_branch() {
        let p = RacePolicy {
            losers: Losers::Prune,
            ..Default::default()
        };
        let s = race(vec![
            done(OK, (1, 1), Some(9), Some(1), 0),
            done(OK, (1, 1), Some(1), Some(1), 1),
            SeatRun::Unlaunched,
            done(ExitStatus::Failed, (0, 1), None, None, 2),
        ]);
        let r = decided(step(&p, &s));
        assert_eq!(r.winner, Some(2));
        assert_eq!(r.prunable(), vec![1, 4]);
        let keep = decided(step(&RacePolicy::default(), &s));
        assert!(keep.prunable().is_empty());
    }

    #[test]
    fn an_unlaunched_seat_is_reported_and_never_waited_on() {
        let s = race(vec![
            SeatRun::Unlaunched,
            done(OK, (1, 1), Some(1), Some(1), 0),
        ]);
        let r = decided(step(&RacePolicy::default(), &s));
        assert_eq!(
            verdicts(&r),
            vec![SeatVerdict::Unlaunched, SeatVerdict::Won]
        );
        assert_eq!(r.seats[0].status, None);
    }

    #[test]
    fn first_stops_the_rest_once_one_passes_then_decides() {
        let p = RacePolicy {
            first: true,
            ..Default::default()
        };
        let waiting = race(vec![
            SeatRun::Running,
            done(OK, (0, 1), Some(1), Some(1), 0),
            SeatRun::Running,
        ]);
        assert_eq!(step(&p, &waiting), Step::Wait);

        let passed = race(vec![
            SeatRun::Running,
            done(OK, (1, 1), Some(1), Some(1), 0),
            SeatRun::Running,
        ]);
        assert_eq!(step(&p, &passed), Step::Stop(vec![1, 3]));

        let stopped = race(vec![
            done(ExitStatus::Killed, (0, 0), None, Some(2), 1),
            done(OK, (1, 1), Some(1), Some(1), 0),
            done(ExitStatus::Cancelled, (0, 0), None, Some(2), 2),
        ]);
        let r = decided(step(&p, &stopped));
        assert_eq!((r.winner, r.decided_by), (Some(2), DecidedBy::FirstPass));
        assert_eq!(
            verdicts(&r),
            vec![
                SeatVerdict::Cancelled,
                SeatVerdict::Won,
                SeatVerdict::Cancelled
            ]
        );
    }

    #[test]
    fn first_picks_the_earliest_passer_even_if_a_later_one_is_cheaper() {
        let p = RacePolicy {
            first: true,
            ..Default::default()
        };
        let s = race(vec![
            done(OK, (1, 1), Some(1), Some(9), 1),
            done(OK, (1, 1), Some(900), Some(1), 0),
        ]);
        let r = decided(step(&p, &s));
        assert_eq!(r.winner, Some(2));
        assert_eq!(verdicts(&r), vec![SeatVerdict::Lost, SeatVerdict::Won]);
    }

    #[test]
    fn first_with_nobody_passing_is_no_pass() {
        let p = RacePolicy {
            first: true,
            ..Default::default()
        };
        let s = race(vec![
            done(ExitStatus::Failed, (0, 1), None, None, 0),
            done(OK, (0, 1), None, None, 1),
        ]);
        assert_eq!(decided(step(&p, &s)).decided_by, DecidedBy::NoPass);
    }

    #[test]
    fn an_abandoned_race_stops_its_seats_then_decides_nobody() {
        let mut s = race(vec![
            SeatRun::Running,
            done(OK, (1, 1), Some(1), Some(1), 0),
        ]);
        s.abandoned = true;
        assert_eq!(step(&RacePolicy::default(), &s), Step::Stop(vec![1]));
        s.seats[0].run = done(ExitStatus::Killed, (0, 0), None, None, 1);
        let r = decided(step(&RacePolicy::default(), &s));
        assert_eq!((r.winner, r.decided_by), (None, DecidedBy::Abandoned));
        assert_eq!(
            verdicts(&r),
            vec![SeatVerdict::Cancelled, SeatVerdict::Lost]
        );
    }

    #[test]
    fn step_is_idempotent() {
        let s = race(vec![
            done(OK, (1, 1), Some(5), Some(5), 0),
            done(OK, (1, 1), Some(4), Some(5), 1),
        ]);
        assert_eq!(
            step(&RacePolicy::default(), &s),
            step(&RacePolicy::default(), &s)
        );
    }

    #[test]
    fn the_scoreboard_reads_like_the_design() {
        let s = race(vec![
            done(ExitStatus::TimedOut, (0, 3), None, Some(600), 0),
            done(OK, (3, 3), Some(41_200), Some(192), 1),
            SeatRun::Unlaunched,
            done(OK, (2, 3), Some(1_250_000), Some(3_725), 2),
        ]);
        let mut r = decided(step(&RacePolicy::default(), &s));
        r.seats[3].pruned = true;
        assert_eq!(
            r.scoreboard(),
            "#1 canned:m1 timed out 0/3 10m00s marion/t-0\n\
             #2 canned:m2 ★ pass 3/3 41k tok 3m12s marion/t-1\n\
             #3 canned:m3 unlaunched\n\
             #4 canned:m4 fail 2/3 1.2M tok 1h02m marion/t-2 (pruned)\n\
             decided by: verification"
        );
    }

    #[test]
    fn a_result_round_trips_and_names_no_prompt() {
        let s = race(vec![
            done(OK, (1, 1), Some(5), Some(5), 0),
            SeatRun::Unlaunched,
        ]);
        let r = decided(step(&RacePolicy::default(), &s));
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<RaceResult>(&json).unwrap(), r);
        assert!(json.contains(r#""decided_by":"verification""#), "{json}");
        assert_eq!(r.winner_row().unwrap().seat, 1);
    }

    #[test]
    fn short_numbers() {
        assert_eq!(
            [
                tokens_short(999),
                tokens_short(1_000),
                tokens_short(41_999),
                tokens_short(1_999_999)
            ],
            ["999", "1k", "41k", "1.9M"]
        );
        assert_eq!(
            [
                secs_short(0),
                secs_short(59),
                secs_short(60),
                secs_short(3_600)
            ],
            ["0s", "59s", "1m00s", "1h00m"]
        );
    }
}
