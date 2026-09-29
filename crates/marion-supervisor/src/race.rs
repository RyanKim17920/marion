//! **Races, driven**: the supervisor half of [`marion_core::race`].
//!
//! A race is a sibling group of ordinary contracted children, each tagged on its `SpawnIntent` with
//! the race and its seat. Nothing here runs on a thread or a timer of its own: a race is driven
//! from the moments that change it — a seat's thread after `mark_finished`, the spawn that launched
//! it once every seat is started, the requester's own end, and a restart's boot — and each of them
//! does the same thing: gather the race's state from the journal and the seats' contract files,
//! ask [`marion_core::race::step`] what to do, and do it. With no open race the map is empty and
//! nothing wakes.
//!
//! **The journal and the contract files are the state**, and this module's map holds only what
//! they cannot say: that a race's seats are still being launched (so a fast seat's end does not
//! decide a race whose other seats have no intent yet), who asked for it, and that a decision is
//! in progress (so two seats ending together do not decide it twice).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use marion_core::contract::{AgentId, ExitStatus, TaskContract};
use marion_core::journal::RaceDecided;
use marion_core::node::ReapState;
use marion_core::paths::ProjectDir;
use marion_core::race::{
    RaceId, RacePolicy, RaceResult, RaceState, SeatOutcome, SeatRun, SeatState,
};
use marion_core::registry::{ReplayedNode, ReplayedRace};

/// Poison is ignored, as everywhere in the supervisor: the map holds bookkeeping, and a panic on
/// one seat's thread must not stop every other race from being driven.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the journal cannot say about one open race.
#[derive(Debug)]
struct Open {
    requester: AgentId,
    /// Seats are still being launched; the race waits whatever the others did.
    launching: bool,
    /// A decision is being written; a second driver backs off.
    deciding: bool,
    /// Seats already told to stop, so a repeated `Stop` step does not stop them twice.
    stopped: HashSet<AgentId>,
}

/// Every race this supervisor is driving. See the module doc.
#[derive(Debug, Default)]
pub struct Races {
    open: Mutex<HashMap<RaceId, Open>>,
}

/// What a driver should do after [`Races::begin_drive`].
#[derive(Debug, PartialEq, Eq)]
pub enum Drive {
    /// Not a race this supervisor is driving, still launching, or being decided by someone else.
    Skip,
    /// Gather and step; `requester` is who asked.
    Go { requester: AgentId },
}

impl Races {
    /// A race is opened before its first seat's intent, with its launch in progress.
    pub fn open(&self, race_id: &RaceId, requester: &AgentId) {
        lock(&self.open).insert(
            race_id.clone(),
            Open {
                requester: requester.clone(),
                launching: true,
                deciding: false,
                stopped: HashSet::new(),
            },
        );
    }

    /// Every seat has been launched (or refused); the race may now be decided.
    pub fn launched(&self, race_id: &RaceId) {
        if let Some(o) = lock(&self.open).get_mut(race_id) {
            o.launching = false;
        }
    }

    /// The races `agent_id` asked for and that are still open — what its own end abandons.
    pub fn requested_by(&self, agent_id: &AgentId) -> Vec<RaceId> {
        lock(&self.open)
            .iter()
            .filter(|(_, o)| &o.requester == agent_id)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn is_open(&self, race_id: &RaceId) -> bool {
        lock(&self.open).contains_key(race_id)
    }

    /// Claim the right to step this race now.
    pub fn begin_drive(&self, race_id: &RaceId) -> Drive {
        let mut open = lock(&self.open);
        match open.get_mut(race_id) {
            Some(o) if !o.launching && !o.deciding => {
                o.deciding = true;
                Drive::Go {
                    requester: o.requester.clone(),
                }
            }
            _ => Drive::Skip,
        }
    }

    /// The step said wait or stop: release the claim.
    pub fn end_drive(&self, race_id: &RaceId) {
        if let Some(o) = lock(&self.open).get_mut(race_id) {
            o.deciding = false;
        }
    }

    /// Of `seats`, the ones not yet told to stop, now marked as told.
    pub fn to_stop(&self, race_id: &RaceId, seats: Vec<AgentId>) -> Vec<AgentId> {
        let mut open = lock(&self.open);
        let Some(o) = open.get_mut(race_id) else {
            return Vec::new();
        };
        seats
            .into_iter()
            .filter(|a| o.stopped.insert(a.clone()))
            .collect()
    }

    /// A seat marked by [`Self::to_stop`] could not be stopped yet; a later drive may try again.
    pub fn forget_stop(&self, race_id: &RaceId, agent_id: &AgentId) {
        if let Some(o) = lock(&self.open).get_mut(race_id) {
            o.stopped.remove(agent_id);
        }
    }

    /// The race is decided and its record written.
    pub fn close(&self, race_id: &RaceId) {
        lock(&self.open).remove(race_id);
    }
}

/// **A race's state, from the journal and the seats' contract files alone.**
///
/// `None` for a race the journal has no opening record for, since the policy and seats come from
/// it. `requester_running` decides abandonment. A seat is:
///
/// * **running** while its node is live on this supervisor's record;
/// * **finished** with its contract's evidence once the contract file exists;
/// * **cancelled** where marion lost it (a restart orphaned it) or it ended with no contract after
///   its process existed;
/// * **unlaunched** where no node took the seat, or its node was aborted before a process existed.
pub fn gather(
    nodes: &[ReplayedNode],
    project: &ProjectDir,
    race: &ReplayedRace,
    requester: &AgentId,
    requester_running: bool,
) -> Option<(RacePolicy, RaceState)> {
    let opened = race.opened.as_ref()?;
    let mut seats = Vec::with_capacity(opened.seats.len());
    let mut any_running = false;
    for (i, candidate) in opened.seats.iter().enumerate() {
        let seat = u8::try_from(i + 1).ok()?;
        let node = race
            .seats
            .iter()
            .find(|(s, _)| *s == seat)
            .and_then(|(_, id)| nodes.iter().find(|n| &n.agent_id == id));
        let intent = node.and_then(|n| n.intent.as_ref());
        let run = match node {
            None => SeatRun::Unlaunched,
            Some(n) if n.spawn_aborted.is_some() && !n.spawn_confirmed => SeatRun::Unlaunched,
            Some(n) => {
                let contract = intent
                    .and_then(|i| i.task_id.as_ref())
                    .and_then(|t| read_contract(&project.agent(&n.agent_id).contract(t)));
                match contract {
                    Some(c) => SeatRun::Finished(outcome(&c)),
                    None if n.reap_state == ReapState::Orphaned || n.state.is_exited() => {
                        SeatRun::Finished(lost())
                    }
                    None if n.spawn_aborted.is_some() => SeatRun::Finished(lost()),
                    None => {
                        any_running = true;
                        SeatRun::Running
                    }
                }
            }
        };
        seats.push(SeatState {
            seat,
            candidate: candidate.clone(),
            harness: intent.map(|i| i.harness),
            agent_id: node.map(|n| n.agent_id.clone()),
            task_id: intent.and_then(|i| i.task_id.clone()),
            run,
        });
    }
    Some((
        opened.policy.clone(),
        RaceState {
            race_id: race.race_id.clone(),
            requester: requester.clone(),
            seats,
            abandoned: any_running && !requester_running,
        },
    ))
}

fn read_contract(path: &Path) -> Option<TaskContract> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A seat marion has no contract for: it ended, but nothing it did can be judged.
fn lost() -> SeatOutcome {
    SeatOutcome {
        status: ExitStatus::Cancelled,
        verified: (0, 0),
        tokens: None,
        secs: None,
        branch: None,
        order: u64::MAX,
    }
}

/// A finished seat's evidence off its contract: status, verification lines passed of those run,
/// the tokens its harness reported, how long it took and where its work landed.
pub fn outcome(c: &TaskContract) -> SeatOutcome {
    let completion = c.completion.as_ref();
    let status = completion.map_or(ExitStatus::Unreported, |c| c.status);
    let (passed, total) = completion.map_or((0, 0), |c| {
        let passed = c
            .evidence
            .iter()
            .filter(|o| o.exit_code == Some(0) && !o.timed_out)
            .count();
        // Evidence the cap dropped is counted as run; an `Ok` contract means every line passed,
        // since any failing line fails the contract.
        let omitted = c.evidence_omitted;
        let passed = passed
            + if c.status == ExitStatus::Ok {
                omitted
            } else {
                0
            };
        (passed, c.evidence.len() + omitted)
    });
    let clamp = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
    let exited = c.timestamps.exited.map(|t| t.0);
    SeatOutcome {
        status,
        verified: (clamp(passed), clamp(total)),
        tokens: completion.and_then(|c| c.usage).map(|u| u.total()),
        secs: exited
            .and_then(|e| e.duration_since(c.timestamps.spawned.0).ok())
            .map(|d| d.as_secs()),
        branch: completion.and_then(|c| c.branch.clone()),
        order: exited
            .and_then(|e| e.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(u64::MAX, |d| {
                u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
            }),
    }
}

/// **Delete a losing seat's branch, and nothing else.** Only the branch its own contract recorded,
/// only one marion names a task branch (`marion/<task_id>`, one segment), and only while it still
/// points at the commit that contract recorded: `update-ref -d <ref> <commit>` is git's
/// compare-and-delete, so a branch someone moved since survives. `true` once it is gone.
pub fn prune_branch(repo: &Path, project: &ProjectDir, row: &marion_core::race::ScoreRow) -> bool {
    let (Some(agent_id), Some(task_id), Some(branch)) = (&row.agent_id, &row.task_id, &row.branch)
    else {
        return false;
    };
    let Some(completion) =
        read_contract(&project.agent(agent_id).contract(task_id)).and_then(|c| c.completion)
    else {
        return false;
    };
    let (Some(recorded), Some(commit)) = (completion.branch, completion.commit) else {
        return false;
    };
    let Some(task_ref) = crate::run::task_branch_ref(branch).filter(|_| &recorded == branch) else {
        return false;
    };
    let _serialized = crate::spawn::repo_write_guard();
    let mut command = std::process::Command::new("git");
    command
        .current_dir(repo)
        .args(["update-ref", "-d", &task_ref, &commit.0])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut command)
        .and_then(|mut c| c.wait())
        .is_ok_and(|s| s.success())
}

/// `races/<race_id>.json`, replaced whole through [`crate::private_fs::write_atomic`] (owner-only,
/// fsynced, renamed into place), so a reader never sees half a scoreboard.
pub fn write_result(project: &ProjectDir, result: &RaceResult) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(result).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    crate::private_fs::write_atomic(&project.race(&result.race_id), &bytes)
}

/// A decided race's scoreboard, as written; `None` while it is open or unreadable.
pub fn read_result(project: &ProjectDir, race_id: &RaceId) -> Option<RaceResult> {
    serde_json::from_slice(&std::fs::read(project.race(race_id)).ok()?).ok()
}

/// The journal's record of a result: the winner's node, what decided it, every seat's verdict.
pub fn decided_record(result: &RaceResult) -> RaceDecided {
    RaceDecided {
        race_id: result.race_id.clone(),
        winner: result.winner_row().and_then(|r| r.agent_id.clone()),
        decided_by: result.decided_by,
        verdicts: result.seats.iter().map(|r| (r.seat, r.verdict)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_race_is_not_driven_while_launching_or_twice_at_once() {
        let races = Races::default();
        let r = RaceId("r".into());
        let p = AgentId("p".into());
        assert_eq!(races.begin_drive(&r), Drive::Skip);
        races.open(&r, &p);
        assert_eq!(races.begin_drive(&r), Drive::Skip, "still launching");
        races.launched(&r);
        assert_eq!(
            races.begin_drive(&r),
            Drive::Go {
                requester: p.clone()
            }
        );
        assert_eq!(races.begin_drive(&r), Drive::Skip, "being decided");
        races.end_drive(&r);
        assert_eq!(
            races.begin_drive(&r),
            Drive::Go {
                requester: p.clone()
            }
        );
        assert_eq!(races.requested_by(&p), vec![r.clone()]);
        races.close(&r);
        assert!(!races.is_open(&r));
        assert!(races.requested_by(&p).is_empty());
    }

    #[test]
    fn a_seat_is_told_to_stop_once() {
        let races = Races::default();
        let r = RaceId("r".into());
        races.open(&r, &AgentId("p".into()));
        let a = AgentId("a".into());
        let b = AgentId("b".into());
        assert_eq!(races.to_stop(&r, vec![a.clone()]), vec![a.clone()]);
        assert_eq!(races.to_stop(&r, vec![a, b.clone()]), vec![b]);
    }
}
