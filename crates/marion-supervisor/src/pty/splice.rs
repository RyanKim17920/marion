//! Atomic replay-to-live cursor contract for the supervisor-owned PTY byte stream.

// Replay remains production-dark; the supervisor currently feeds only the retained prefix.
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DisplayKind {
    Output(Arc<[u8]>),
    Resize { rows: u16, cols: u16 },
    End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DisplayRecord {
    pub(super) seq: u64,
    pub(super) kind: DisplayKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SpliceLimits {
    retained_bytes: usize,
    max_retained_records: usize,
    subscribers: usize,
    queued_records_per_subscriber: usize,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum SpliceError {
    #[error("PTY display stream has ended")]
    Ended,
    #[error("PTY retained output exceeds the {limit}-byte limit")]
    RetainedBytesExceeded { limit: usize },
    #[error("PTY display sequence is exhausted")]
    SequenceExhausted,
    #[error("PTY subscriber limit {limit} is exhausted")]
    SubscriberLimitExceeded { limit: usize },
    #[error("PTY subscriber id sequence is exhausted")]
    SubscriberIdExhausted,
    #[error("PTY subscriber {0:?} is gone")]
    SubscriberGone(SubscriberId),
    #[error("PTY subscriber {0:?} overflowed")]
    SubscriberOverflowed(SubscriberId),
    #[error("PTY subscriber is not in the required replay phase")]
    WrongSubscriberPhase,
    #[error("PTY retained record limit {limit} is exhausted")]
    RetainedRecordsExceeded { limit: usize },
}

#[derive(Debug, PartialEq, Eq)]
enum DeliveryError<E> {
    Splice(SpliceError),
    Callback(E),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmitOutcome {
    wake_ready: Vec<SubscriberId>,
    /// These subscribers stopped accepting records immediately. This is not a completed removal:
    /// one record already reserved by an active callback may finish before its token observes the
    /// overflow and removes the entry.
    overflowed: Vec<SubscriberId>,
}

pub(super) struct PtySplice {
    inner: Arc<Inner>,
}

struct Inner {
    limits: SpliceLimits,
    state: Mutex<State>,
}

struct State {
    records: Vec<DisplayRecord>,
    retained_bytes: usize,
    next_seq: u64,
    next_subscriber: u64,
    subscribers: BTreeMap<SubscriberId, SubscriberState>,
    ended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct SubscriberId(u64);

struct SubscriberState {
    phase: SubscriberPhase,
    removing: bool,
}

enum SubscriberPhase {
    Replaying(VecDeque<DisplayRecord>),
    Ready(VecDeque<DisplayRecord>),
}

impl SubscriberPhase {
    fn queue(&self) -> &VecDeque<DisplayRecord> {
        match self {
            Self::Replaying(queue) | Self::Ready(queue) => queue,
        }
    }

    fn queue_mut(&mut self) -> &mut VecDeque<DisplayRecord> {
        match self {
            Self::Replaying(queue) | Self::Ready(queue) => queue,
        }
    }

    fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

struct ReplaySubscription {
    core: Option<SubscriptionCore>,
    prefix: Vec<DisplayRecord>,
    cut: u64,
}

#[derive(Clone)]
struct SubscriptionCore {
    inner: Arc<Inner>,
    id: SubscriberId,
}

impl SubscriptionCore {
    fn remove(&self) -> bool {
        lock_recover(&self.inner.state)
            .subscribers
            .remove(&self.id)
            .is_some()
    }
}

fn subscriber_id(core: &Option<SubscriptionCore>) -> Result<SubscriberId, SpliceError> {
    core.as_ref()
        .map(|core| core.id)
        .ok_or(SpliceError::SubscriberGone(SubscriberId(0)))
}

fn remove_subscription(core: &mut Option<SubscriptionCore>) -> bool {
    core.take().is_some_and(|core| core.remove())
}

impl ReplaySubscription {
    fn id(&self) -> Result<SubscriberId, SpliceError> {
        subscriber_id(&self.core)
    }

    fn remove(mut self) -> bool {
        remove_subscription(&mut self.core)
    }

    fn cut(&self) -> u64 {
        self.cut
    }

    fn replay_with<E, F>(mut self, mut callback: F) -> Result<ReadySubscription, DeliveryError<E>>
    where
        F: FnMut(DisplayRecord) -> Result<(), E>,
    {
        let Some(core) = self.core.as_ref().cloned() else {
            return Err(DeliveryError::Splice(SpliceError::SubscriberGone(
                SubscriberId(0),
            )));
        };

        for record in std::mem::take(&mut self.prefix) {
            if let Err(error) = reserve_replay_record(&core) {
                remove_subscription(&mut self.core);
                return Err(DeliveryError::Splice(error));
            }
            if let Err(error) = callback(record) {
                remove_subscription(&mut self.core);
                return Err(DeliveryError::Callback(error));
            }
        }

        loop {
            let step = match replay_step(&core) {
                Ok(step) => step,
                Err(error) => {
                    remove_subscription(&mut self.core);
                    return Err(DeliveryError::Splice(error));
                }
            };
            match step {
                ReplayStep::Record(record) => {
                    if let Err(error) = callback(record) {
                        remove_subscription(&mut self.core);
                        return Err(DeliveryError::Callback(error));
                    }
                }
                ReplayStep::Ready => break,
            }
        }

        self.core.take();
        Ok(ReadySubscription { core: Some(core) })
    }
}

struct ReadySubscription {
    core: Option<SubscriptionCore>,
}

impl ReadySubscription {
    fn id(&self) -> Result<SubscriberId, SpliceError> {
        subscriber_id(&self.core)
    }

    fn remove(mut self) -> bool {
        remove_subscription(&mut self.core)
    }

    fn drain_with<E, F>(&mut self, mut callback: F) -> Result<usize, DeliveryError<E>>
    where
        F: FnMut(DisplayRecord) -> Result<(), E>,
    {
        let Some(core) = self.core.as_ref().cloned() else {
            return Err(DeliveryError::Splice(SpliceError::SubscriberGone(
                SubscriberId(0),
            )));
        };
        let mut delivered = 0;
        loop {
            let record = match ready_record(&core) {
                Ok(record) => record,
                Err(error) => {
                    remove_subscription(&mut self.core);
                    return Err(DeliveryError::Splice(error));
                }
            };
            let Some(record) = record else {
                return Ok(delivered);
            };
            if let Err(error) = callback(record) {
                remove_subscription(&mut self.core);
                return Err(DeliveryError::Callback(error));
            }
            delivered += 1;
        }
    }
}

impl Drop for ReadySubscription {
    fn drop(&mut self) {
        remove_subscription(&mut self.core);
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

enum ReplayStep {
    Record(DisplayRecord),
    Ready,
}

fn active_subscriber(
    state: &mut State,
    id: SubscriberId,
) -> Result<&mut SubscriberState, SpliceError> {
    let Some(subscriber) = state.subscribers.get(&id) else {
        return Err(SpliceError::SubscriberGone(id));
    };
    if subscriber.removing {
        state.subscribers.remove(&id);
        return Err(SpliceError::SubscriberOverflowed(id));
    }
    Ok(state
        .subscribers
        .get_mut(&id)
        .expect("subscriber was checked above"))
}

fn reserve_replay_record(core: &SubscriptionCore) -> Result<(), SpliceError> {
    let mut state = lock_recover(&core.inner.state);
    let subscriber = active_subscriber(&mut state, core.id)?;
    if matches!(subscriber.phase, SubscriberPhase::Replaying(_)) {
        Ok(())
    } else {
        Err(SpliceError::WrongSubscriberPhase)
    }
}

fn replay_step(core: &SubscriptionCore) -> Result<ReplayStep, SpliceError> {
    let mut state = lock_recover(&core.inner.state);
    let subscriber = active_subscriber(&mut state, core.id)?;
    match &mut subscriber.phase {
        SubscriberPhase::Replaying(backlog) if backlog.is_empty() => {
            subscriber.phase = SubscriberPhase::Ready(VecDeque::new());
            Ok(ReplayStep::Ready)
        }
        SubscriberPhase::Replaying(backlog) => backlog
            .pop_front()
            .map(ReplayStep::Record)
            .ok_or(SpliceError::WrongSubscriberPhase),
        SubscriberPhase::Ready(_) => Err(SpliceError::WrongSubscriberPhase),
    }
}

fn ready_record(core: &SubscriptionCore) -> Result<Option<DisplayRecord>, SpliceError> {
    let mut state = lock_recover(&core.inner.state);
    let subscriber = active_subscriber(&mut state, core.id)?;
    match &mut subscriber.phase {
        SubscriberPhase::Ready(backlog) => Ok(backlog.pop_front()),
        SubscriberPhase::Replaying(_) => Err(SpliceError::WrongSubscriberPhase),
    }
}

impl Drop for ReplaySubscription {
    fn drop(&mut self) {
        remove_subscription(&mut self.core);
    }
}

impl PtySplice {
    pub(super) fn new(limits: SpliceLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                limits,
                state: Mutex::new(State {
                    records: Vec::new(),
                    retained_bytes: 0,
                    next_seq: 1,
                    next_subscriber: 1,
                    subscribers: BTreeMap::new(),
                    ended: false,
                }),
            }),
        }
    }

    pub(super) fn retain_output(&self, bytes: Arc<[u8]>) -> Result<(), SpliceError> {
        self.emit_output(bytes).map(|_| ())
    }

    pub(super) fn retain_resize(&self, rows: u16, cols: u16) -> Result<(), SpliceError> {
        self.emit_resize(rows, cols).map(|_| ())
    }

    pub(super) fn retain_end(&self) -> Result<(), SpliceError> {
        self.emit_end().map(|_| ())
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Vec<DisplayRecord> {
        lock_recover(&self.inner.state).records.clone()
    }

    fn begin_replay(&self) -> Result<ReplaySubscription, SpliceError> {
        let mut state = lock_recover(&self.inner.state);
        if state.subscribers.len() >= self.inner.limits.subscribers {
            return Err(SpliceError::SubscriberLimitExceeded {
                limit: self.inner.limits.subscribers,
            });
        }
        let id = SubscriberId(state.next_subscriber);
        let next_subscriber = state
            .next_subscriber
            .checked_add(1)
            .ok_or(SpliceError::SubscriberIdExhausted)?;
        let prefix = state.records.clone();
        let cut = prefix.last().map_or(0, |record| record.seq);
        state.subscribers.insert(
            id,
            SubscriberState {
                phase: SubscriberPhase::Replaying(VecDeque::new()),
                removing: false,
            },
        );
        state.next_subscriber = next_subscriber;
        Ok(ReplaySubscription {
            core: Some(SubscriptionCore {
                inner: Arc::clone(&self.inner),
                id,
            }),
            prefix,
            cut,
        })
    }

    fn emit_output(&self, bytes: Arc<[u8]>) -> Result<EmitOutcome, SpliceError> {
        let byte_len = bytes.len();
        self.emit(DisplayKind::Output(bytes), byte_len)
    }

    fn emit_resize(&self, rows: u16, cols: u16) -> Result<EmitOutcome, SpliceError> {
        self.emit(DisplayKind::Resize { rows, cols }, 0)
    }

    fn emit_end(&self) -> Result<EmitOutcome, SpliceError> {
        self.emit(DisplayKind::End, 0)
    }

    fn emit(&self, kind: DisplayKind, added_bytes: usize) -> Result<EmitOutcome, SpliceError> {
        let mut state = lock_recover(&self.inner.state);
        if state.ended {
            return Err(SpliceError::Ended);
        }
        if state.records.len() >= self.inner.limits.max_retained_records {
            return Err(SpliceError::RetainedRecordsExceeded {
                limit: self.inner.limits.max_retained_records,
            });
        }
        let retained_bytes = state
            .retained_bytes
            .checked_add(added_bytes)
            .filter(|retained| *retained <= self.inner.limits.retained_bytes)
            .ok_or(SpliceError::RetainedBytesExceeded {
                limit: self.inner.limits.retained_bytes,
            })?;
        let seq = state.next_seq;
        let next_seq = seq.checked_add(1).ok_or(SpliceError::SequenceExhausted)?;
        let is_end = matches!(&kind, DisplayKind::End);
        let record = DisplayRecord { seq, kind };

        state.records.push(record.clone());
        state.retained_bytes = retained_bytes;
        state.next_seq = next_seq;
        state.ended = is_end;

        let mut wake_ready = Vec::new();
        let mut overflowed = Vec::new();
        for (&id, subscriber) in &mut state.subscribers {
            if subscriber.removing {
                continue;
            }
            if subscriber.phase.queue().len() >= self.inner.limits.queued_records_per_subscriber {
                subscriber.removing = true;
                subscriber.phase.queue_mut().clear();
                overflowed.push(id);
                continue;
            }
            let ready = subscriber.phase.is_ready();
            subscriber.phase.queue_mut().push_back(record.clone());
            if ready {
                wake_ready.push(id);
            }
        }
        Ok(EmitOutcome {
            wake_ready,
            overflowed,
        })
    }
}

impl SpliceLimits {
    pub(super) const fn production() -> Self {
        Self {
            retained_bytes: 64 * 1024 * 1024,
            max_retained_records: 1_000_000,
            subscribers: 64,
            queued_records_per_subscriber: 16_384,
        }
    }

    #[cfg(test)]
    pub(super) const fn testing(retained_bytes: usize, max_retained_records: usize) -> Self {
        Self {
            retained_bytes,
            max_retained_records,
            subscribers: 0,
            queued_records_per_subscriber: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{mpsc, Barrier};
    use std::time::Duration;

    use super::*;

    #[test]
    fn pty_splice_constructs_with_explicit_limits() {
        let limits = SpliceLimits {
            retained_bytes: 1024,
            max_retained_records: 16,
            subscribers: 4,
            queued_records_per_subscriber: 8,
        };
        let splice = PtySplice::new(limits);

        assert_eq!(splice.inner.limits, limits);
        assert!(splice.inner.state.lock().is_ok());
    }

    #[test]
    fn pty_splice_emits_dense_output_resize_and_end_with_shallow_bytes() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 32,
            max_retained_records: 4,
            subscribers: 0,
            queued_records_per_subscriber: 0,
        });
        let bytes: Arc<[u8]> = Arc::from(&b"\x1b[?1049h\xff"[..]);

        splice.emit_output(Arc::clone(&bytes)).unwrap();
        splice.emit_resize(24, 80).unwrap();
        splice.emit_end().unwrap();

        let state = lock_recover(&splice.inner.state);
        assert_eq!(
            state
                .records
                .iter()
                .map(|record| record.seq)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let DisplayKind::Output(retained) = &state.records[0].kind else {
            panic!("first record must be output");
        };
        assert!(Arc::ptr_eq(retained, &bytes));
        assert_eq!(
            state.records[1].kind,
            DisplayKind::Resize { rows: 24, cols: 80 }
        );
        assert_eq!(state.records[2].kind, DisplayKind::End);
    }

    #[test]
    fn pty_splice_retention_refusal_does_not_advance_state() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 3,
            max_retained_records: 4,
            subscribers: 0,
            queued_records_per_subscriber: 0,
        });
        splice.emit_output(Arc::from(&b"abc"[..])).unwrap();

        assert_eq!(
            splice.emit_output(Arc::from(&b"x"[..])),
            Err(SpliceError::RetainedBytesExceeded { limit: 3 })
        );
        splice.emit_resize(1, 2).unwrap();
        let state = splice
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.records.len(), 2);
        assert_eq!(state.records[1].seq, 2);
        assert_eq!(state.retained_bytes, 3);
    }

    #[test]
    fn pty_splice_end_and_sequence_exhaustion_are_terminal_without_advance() {
        let ended = PtySplice::new(SpliceLimits {
            retained_bytes: 0,
            max_retained_records: 2,
            subscribers: 0,
            queued_records_per_subscriber: 0,
        });
        ended.emit_end().unwrap();
        assert_eq!(ended.emit_end(), Err(SpliceError::Ended));
        assert_eq!(
            ended.emit_output(Arc::from(&b"late"[..])),
            Err(SpliceError::Ended)
        );

        let exhausted = PtySplice::new(SpliceLimits {
            retained_bytes: 0,
            max_retained_records: 2,
            subscribers: 0,
            queued_records_per_subscriber: 0,
        });
        exhausted
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next_seq = u64::MAX;
        assert_eq!(
            exhausted.emit_resize(1, 1),
            Err(SpliceError::SequenceExhausted)
        );
        let state = exhausted
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.records.is_empty());
        assert_eq!(state.next_seq, u64::MAX);
    }

    #[test]
    fn pty_splice_emit_queues_replaying_without_wake() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 8,
            max_retained_records: 4,
            subscribers: 1,
            queued_records_per_subscriber: 2,
        });
        let replay = splice.begin_replay().unwrap();
        let bytes: Arc<[u8]> = Arc::from(&b"raw"[..]);

        let outcome = splice.emit_output(Arc::clone(&bytes)).unwrap();
        assert_eq!(
            outcome,
            EmitOutcome {
                wake_ready: vec![],
                overflowed: vec![]
            }
        );
        let state = lock_recover(&splice.inner.state);
        let subscriber = state.subscribers.get(&replay.id().unwrap()).unwrap();
        let SubscriberPhase::Replaying(queue) = &subscriber.phase else {
            panic!("subscriber must remain replaying");
        };
        assert_eq!(queue.len(), 1);
        let DisplayKind::Output(queued) = &queue[0].kind else {
            panic!("queued record must retain output");
        };
        assert!(Arc::ptr_eq(queued, &bytes));
    }

    #[test]
    fn pty_splice_emit_queues_and_wakes_synthetic_ready_fifo() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 8,
            max_retained_records: 4,
            subscribers: 1,
            queued_records_per_subscriber: 3,
        });
        let replay = splice.begin_replay().unwrap();
        let id = replay.id().unwrap();
        lock_recover(&splice.inner.state)
            .subscribers
            .get_mut(&id)
            .unwrap()
            .phase = SubscriberPhase::Ready(VecDeque::new());

        assert_eq!(
            splice.emit_output(Arc::from(&b"a"[..])).unwrap().wake_ready,
            [id]
        );
        assert_eq!(splice.emit_resize(24, 80).unwrap().wake_ready, [id]);
        let state = lock_recover(&splice.inner.state);
        let SubscriberPhase::Ready(queue) = &state.subscribers.get(&id).unwrap().phase else {
            panic!("subscriber must remain ready");
        };
        assert_eq!(
            queue.iter().map(|record| record.seq).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn pty_splice_queue_cap_evicts_and_later_emits_omit_subscriber() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 8,
            max_retained_records: 4,
            subscribers: 1,
            queued_records_per_subscriber: 1,
        });
        let replay = splice.begin_replay().unwrap();
        let id = replay.id().unwrap();
        assert!(splice
            .emit_output(Arc::from(&b"a"[..]))
            .unwrap()
            .overflowed
            .is_empty());

        let overflow = splice.emit_output(Arc::from(&b"b"[..])).unwrap();
        assert_eq!(overflow.overflowed, [id]);
        assert!(overflow.wake_ready.is_empty());
        {
            let state = lock_recover(&splice.inner.state);
            let subscriber = state.subscribers.get(&id).unwrap();
            assert!(subscriber.removing);
            assert!(subscriber.phase.queue().is_empty());
        }

        let later = splice.emit_resize(1, 1).unwrap();
        assert!(later.wake_ready.is_empty());
        assert!(later.overflowed.is_empty());
    }

    #[test]
    fn pty_splice_begin_replay_registers_exact_shallow_prefix_and_cut() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 32,
            max_retained_records: 4,
            subscribers: 1,
            queued_records_per_subscriber: 4,
        });
        let bytes: Arc<[u8]> = Arc::from(&b"\x1b[?1049h\xff"[..]);
        splice.emit_output(Arc::clone(&bytes)).unwrap();
        splice.emit_resize(24, 80).unwrap();

        let replay = splice.begin_replay().unwrap();
        assert_eq!(replay.cut(), 2);
        assert_eq!(replay.prefix.len(), 2);
        let DisplayKind::Output(prefix_bytes) = &replay.prefix[0].kind else {
            panic!("prefix starts with output");
        };
        assert!(Arc::ptr_eq(prefix_bytes, &bytes));
        assert_eq!(
            replay.prefix[1].kind,
            DisplayKind::Resize { rows: 24, cols: 80 }
        );
        let state = splice
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(matches!(
            state.subscribers.get(&replay.id().unwrap()),
            Some(SubscriberState {
                phase: SubscriberPhase::Replaying(backlog),
                ..
            }) if backlog.is_empty()
        ));
    }

    #[test]
    fn pty_splice_subscriber_bound_refusal_leaves_registration_state_unchanged() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 0,
            max_retained_records: 2,
            subscribers: 1,
            queued_records_per_subscriber: 0,
        });
        let first = splice.begin_replay().unwrap();
        assert!(matches!(
            splice.begin_replay(),
            Err(SpliceError::SubscriberLimitExceeded { limit: 1 })
        ));
        let state = splice
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.subscribers.len(), 1);
        assert_eq!(state.next_subscriber, 2);
        assert!(state.subscribers.contains_key(&first.id().unwrap()));
    }

    #[test]
    fn pty_splice_subscriber_id_exhaustion_does_not_register() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 0,
            max_retained_records: 2,
            subscribers: 1,
            queued_records_per_subscriber: 0,
        });
        splice
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next_subscriber = u64::MAX;

        assert!(matches!(
            splice.begin_replay(),
            Err(SpliceError::SubscriberIdExhausted)
        ));
        let state = splice
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.subscribers.is_empty());
        assert_eq!(state.next_subscriber, u64::MAX);
    }

    #[test]
    fn pty_splice_replay_token_drop_removes_registration_idempotently() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 0,
            max_retained_records: 2,
            subscribers: 1,
            queued_records_per_subscriber: 0,
        });
        let replay = splice.begin_replay().unwrap();
        let id = replay.id().unwrap();
        drop(replay);
        {
            let state = splice
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(!state.subscribers.contains_key(&id));
        }

        let replay = splice.begin_replay().unwrap();
        assert!(replay.remove());
        assert!(splice
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscribers
            .is_empty());
    }

    #[test]
    fn pty_splice_emit_during_prefix_replays_fifo_before_ready_live_drain() {
        let splice = Arc::new(PtySplice::new(SpliceLimits {
            retained_bytes: 64,
            max_retained_records: 8,
            subscribers: 1,
            queued_records_per_subscriber: 8,
        }));
        splice.emit_output(Arc::from(&b"prefix"[..])).unwrap();
        let replay = splice.begin_replay().unwrap();
        let id = replay.id().unwrap();
        let (prefix_started_tx, prefix_started_rx) = mpsc::channel();
        let release_prefix = Arc::new(Barrier::new(2));
        let replay_thread = {
            let release_prefix = Arc::clone(&release_prefix);
            std::thread::spawn(move || {
                let mut observed = Vec::new();
                let ready = replay
                    .replay_with(|record| {
                        observed.push(record.seq);
                        if record.seq == 1 {
                            prefix_started_tx.send(()).unwrap();
                            release_prefix.wait();
                        }
                        Ok::<(), ()>(())
                    })
                    .unwrap();
                (ready, observed)
            })
        };

        prefix_started_rx.recv().unwrap();
        assert!(splice
            .emit_output(Arc::from(&b"queued"[..]))
            .unwrap()
            .wake_ready
            .is_empty());
        assert!(splice.emit_resize(24, 80).unwrap().wake_ready.is_empty());
        release_prefix.wait();
        let (mut ready, mut observed) = replay_thread.join().unwrap();
        assert_eq!(observed, [1, 2, 3]);

        let live = splice.emit_output(Arc::from(&b"live"[..])).unwrap();
        assert_eq!(live.wake_ready, [id]);
        assert_eq!(
            ready
                .drain_with(|record| {
                    observed.push(record.seq);
                    Ok::<(), ()>(())
                })
                .unwrap(),
            1
        );
        assert_eq!(observed, [1, 2, 3, 4]);
        assert!(ready.remove());
        assert!(splice.emit_resize(30, 100).unwrap().wake_ready.is_empty());
    }

    #[test]
    fn pty_splice_callback_failure_removes_replay_and_ready_subscribers() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 64,
            max_retained_records: 8,
            subscribers: 1,
            queued_records_per_subscriber: 8,
        });
        splice.emit_output(Arc::from(&b"prefix"[..])).unwrap();
        let replay = splice.begin_replay().unwrap();
        let replay_id = replay.id().unwrap();
        let replay_result: Result<ReadySubscription, DeliveryError<&'static str>> =
            replay.replay_with(|_| Err("replay send failed"));
        assert!(matches!(
            replay_result,
            Err(DeliveryError::Callback("replay send failed"))
        ));
        assert!(!lock_recover(&splice.inner.state)
            .subscribers
            .contains_key(&replay_id));

        let replay = splice.begin_replay().unwrap();
        let mut ready = replay.replay_with(|_| Ok::<(), &'static str>(())).unwrap();
        let ready_id = ready.id().unwrap();
        splice.emit_resize(24, 80).unwrap();
        assert!(matches!(
            ready.drain_with(|_| Err("ready send failed")),
            Err(DeliveryError::Callback("ready send failed"))
        ));
        assert!(!lock_recover(&splice.inner.state)
            .subscribers
            .contains_key(&ready_id));
        assert!(splice.emit_resize(25, 81).unwrap().wake_ready.is_empty());
    }

    #[test]
    fn pty_splice_ready_drain_stays_monotonic_with_concurrent_emitter() {
        let splice = Arc::new(PtySplice::new(SpliceLimits {
            retained_bytes: 64,
            max_retained_records: 16,
            subscribers: 1,
            queued_records_per_subscriber: 32,
        }));
        let replay = splice.begin_replay().unwrap();
        let mut ready = replay.replay_with(|_| Ok::<(), ()>(())).unwrap();
        splice.emit_output(Arc::from(&b"first"[..])).unwrap();
        let start_emitter = Arc::new(Barrier::new(2));
        let emitter_done = Arc::new(Barrier::new(2));
        let emitter = {
            let splice = Arc::clone(&splice);
            let start_emitter = Arc::clone(&start_emitter);
            let emitter_done = Arc::clone(&emitter_done);
            std::thread::spawn(move || {
                start_emitter.wait();
                for _ in 0..10 {
                    splice.emit_resize(24, 80).unwrap();
                }
                emitter_done.wait();
            })
        };

        let mut observed = Vec::new();
        ready
            .drain_with(|record| {
                observed.push(record.seq);
                if record.seq == 1 {
                    start_emitter.wait();
                    emitter_done.wait();
                }
                Ok::<(), ()>(())
            })
            .unwrap();
        emitter.join().unwrap();
        assert_eq!(observed, (1..=11).collect::<Vec<_>>());
    }

    #[test]
    fn pty_splice_ready_overflow_allows_only_reserved_record_to_finish() {
        let splice = Arc::new(PtySplice::new(SpliceLimits {
            retained_bytes: 64,
            max_retained_records: 8,
            subscribers: 1,
            queued_records_per_subscriber: 2,
        }));
        let replay = splice.begin_replay().unwrap();
        let id = replay.id().unwrap();
        let mut ready = replay.replay_with(|_| Ok::<(), ()>(())).unwrap();
        splice.emit_output(Arc::from(&b"one"[..])).unwrap();
        splice.emit_resize(2, 2).unwrap();
        let (reserved_tx, reserved_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let drainer = std::thread::spawn(move || {
            let mut observed = Vec::new();
            let result = ready.drain_with(|record| {
                observed.push(record.seq);
                if record.seq == 1 {
                    reserved_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                Ok::<(), ()>(())
            });
            let inactive_id = ready.id();
            let second_drain = ready.drain_with(|_| Ok::<(), ()>(()));
            let removed = ready.remove();
            (result, observed, inactive_id, second_drain, removed)
        });

        reserved_rx.recv().unwrap();
        splice.emit_resize(3, 3).unwrap();
        let overflow = splice.emit_resize(4, 4).unwrap();
        assert_eq!(overflow.overflowed, [id]);
        release_tx.send(()).unwrap();
        let (result, observed, inactive_id, second_drain, removed) = drainer.join().unwrap();
        assert!(matches!(
            result,
            Err(DeliveryError::Splice(SpliceError::SubscriberOverflowed(got))) if got == id
        ));
        assert_eq!(observed, [1]);
        assert_eq!(
            inactive_id,
            Err(SpliceError::SubscriberGone(SubscriberId(0)))
        );
        assert_eq!(
            second_drain,
            Err(DeliveryError::Splice(SpliceError::SubscriberGone(
                SubscriberId(0)
            )))
        );
        assert!(!removed);
        assert!(!lock_recover(&splice.inner.state)
            .subscribers
            .contains_key(&id));
    }

    #[test]
    fn pty_splice_replay_prefix_overflow_stops_before_next_prefix_record() {
        let splice = Arc::new(PtySplice::new(SpliceLimits {
            retained_bytes: 64,
            max_retained_records: 4,
            subscribers: 1,
            queued_records_per_subscriber: 1,
        }));
        splice.emit_output(Arc::from(&b"one"[..])).unwrap();
        splice.emit_resize(2, 2).unwrap();
        let replay = splice.begin_replay().unwrap();
        let id = replay.id().unwrap();
        let (reserved_tx, reserved_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let replay_thread = std::thread::spawn(move || {
            let mut observed = Vec::new();
            let result = replay.replay_with(|record| {
                observed.push(record.seq);
                if record.seq == 1 {
                    reserved_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                Ok::<(), ()>(())
            });
            (result, observed)
        });

        reserved_rx.recv().unwrap();
        splice.emit_resize(3, 3).unwrap();
        assert_eq!(splice.emit_resize(4, 4).unwrap().overflowed, [id]);
        release_tx.send(()).unwrap();
        let (result, observed) = replay_thread.join().unwrap();
        assert!(matches!(
            result,
            Err(DeliveryError::Splice(SpliceError::SubscriberOverflowed(got))) if got == id
        ));
        assert_eq!(observed, [1]);
    }

    #[test]
    fn pty_splice_reentrant_callback_overflow_is_bounded_and_nonblocking() {
        let splice = Arc::new(PtySplice::new(SpliceLimits {
            retained_bytes: 64,
            max_retained_records: 4,
            subscribers: 1,
            queued_records_per_subscriber: 1,
        }));
        let replay = splice.begin_replay().unwrap();
        let id = replay.id().unwrap();
        let mut ready = replay.replay_with(|_| Ok::<(), ()>(())).unwrap();
        splice.emit_output(Arc::from(&b"reserved"[..])).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = {
            let splice = Arc::clone(&splice);
            std::thread::spawn(move || {
                let mut observed = Vec::new();
                let result = ready.drain_with(|record| {
                    observed.push(record.seq);
                    splice.emit_resize(2, 2).unwrap();
                    assert_eq!(splice.emit_resize(3, 3).unwrap().overflowed, [id]);
                    Ok::<(), ()>(())
                });
                done_tx.send((result, observed)).unwrap();
            })
        };

        let (result, observed) = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reentrant emit must not deadlock");
        worker.join().unwrap();
        assert!(matches!(
            result,
            Err(DeliveryError::Splice(SpliceError::SubscriberOverflowed(got))) if got == id
        ));
        assert_eq!(observed, [1]);
    }

    #[test]
    fn pty_splice_zero_byte_record_cap_refusal_leaves_state_unchanged() {
        let splice = PtySplice::new(SpliceLimits {
            retained_bytes: 0,
            max_retained_records: 1,
            subscribers: 0,
            queued_records_per_subscriber: 0,
        });
        splice.emit_resize(1, 1).unwrap();
        assert_eq!(
            splice.emit_resize(2, 2),
            Err(SpliceError::RetainedRecordsExceeded { limit: 1 })
        );
        assert_eq!(
            splice.emit_end(),
            Err(SpliceError::RetainedRecordsExceeded { limit: 1 })
        );
        let state = lock_recover(&splice.inner.state);
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.records[0].seq, 1);
        assert_eq!(state.next_seq, 2);
        assert_eq!(state.retained_bytes, 0);
        assert!(!state.ended);
    }

    #[test]
    fn pty_splice_explicit_remove_then_concurrent_emit_has_no_wake_or_queue() {
        let splice = Arc::new(PtySplice::new(SpliceLimits {
            retained_bytes: 8,
            max_retained_records: 2,
            subscribers: 1,
            queued_records_per_subscriber: 1,
        }));
        let replay = splice.begin_replay().unwrap();
        let id = replay.id().unwrap();
        assert!(replay.remove());
        let start = Arc::new(Barrier::new(2));
        let emitter = {
            let splice = Arc::clone(&splice);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                splice.emit_output(Arc::from(&b"after"[..])).unwrap()
            })
        };
        start.wait();
        let outcome = emitter.join().unwrap();
        assert!(outcome.wake_ready.is_empty());
        assert!(outcome.overflowed.is_empty());
        assert!(!lock_recover(&splice.inner.state)
            .subscribers
            .contains_key(&id));
    }
}
