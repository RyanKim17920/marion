use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use marion_core::contract::AgentId;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::result::NodeAttachResult;
use marion_core::proto::{AttachMode, FailureKind, ReplayPoint, RpcError};

use super::delivery::deliver_events;
use super::summary::summarize;
use super::{RegistryHandle, lock};
use crate::serve::{ConnId, Outbound};

/// One client following one node's `events.jsonl`.
///
/// The [`EventReader`](crate::events::EventReader)(crate::events::EventReader) is owned here rather than shared, because a
/// cursor is per subscription: two clients attaching to one node at different moments have
/// different read points, and a shared reader would make the second one's replay depend on when
/// the first attached. The file is the shared thing; the position in it is not.
///
/// `conn` is kept alongside `out` so [`Handle::gone`](crate::serve::Handle::gone) can drop this without asking the transport
/// anything — the same bookkeeping `subs` gets, for the same reason.
///
/// `watch` is what makes the cursor advance: it wakes the accept loop when the file changes, and
/// the loop's tick reads it. `None` only if its thread could not be started, and then
/// [`RegistryHandle::next_deadline`] re-reads at [`crate::wake::DEGRADED_RECHECK`].
pub(super) struct Attachment {
    pub(super) conn: ConnId,
    pub(super) agent_id: AgentId,
    pub(super) reader: crate::events::EventReader,
    pub(super) out: Outbound,
    pub(super) watch: Option<AttachWatch>,
}

/// **A followed node's `events.jsonl`, made into a wake for the accept loop.**
///
/// A thread blocked on a [`crate::wake::Watch`] of the file (kqueue `EVFILT_VNODE` / inotify, so
/// it sees every writer, in this process or not) that notifies the handler's
/// [`LiveRegistry::changes`](crate::registry::LiveRegistry::changes) — the signal the accept loop's wake pipe is attached to — whenever the
/// file is written. An idle attached node therefore costs nothing; a node that is speaking costs a
/// wake per write burst rather than one every few milliseconds whether or not it spoke.
///
/// **Register, then look.** The thread notifies once as soon as the watch is armed, so a record
/// appended between the attach's replay and the arming is read by the pass that wakes; after that,
/// each wake re-arms before it notifies, so a write landing during the pass fires the watch again.
///
/// Dropped with its [`Attachment`]: raising the stop flag wakes the thread, and the drop joins it.
pub(super) struct AttachWatch {
    stop: Arc<crate::wake::Flag>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AttachWatch {
    fn start(path: &std::path::Path, changes: Arc<crate::wake::Signal>) -> Option<AttachWatch> {
        let stop = Arc::new(crate::wake::Flag::new());
        let path = path.to_path_buf();
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("marion-attach-watch".into())
            .spawn(move || {
                let mut watch = crate::wake::Watch::new(&path);
                changes.notify();
                while !flag.load(Ordering::SeqCst) {
                    let ready = crate::wake::wait_until(&[watch.fd(), flag.fd()], None);
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    if ready[0] || watch.fd().is_none() {
                        watch.rearm();
                        changes.notify();
                    }
                }
            })
            .ok()?;
        Some(AttachWatch {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for AttachWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// **Every node this supervisor holds a pty for, and who is typing into each.**
///
/// Separate from [`Shared`](super::Shared) rather than a field of it, for the reason [`RegistryHandle::nodes`]
/// gives: `shared` is taken and released inside one call, and writing a keystroke is a `write(2)`
/// on a pty master. Folding them together would put an operator's keyboard inside the lock every
/// `tree/subscribe` waits on — and a node whose harness has stopped reading its stdin would then
/// block the whole supervisor rather than one attach.
#[derive(Default)]
pub(super) struct Panes {
    /// `Arc` because a keystroke is delivered outside the map's lock: the host is cloned out, the
    /// lock is released, and only then is the byte written. A `&PtyHost` would hold the map for
    /// the duration of the write.
    pub(super) hosts: HashMap<AgentId, PaneEntry>,
    /// Write leases, **keyed by the connection**, which is what makes §7.3.1 automatic here.
    /// `WriteLease` releases the node's writer slot on `Drop`, so a client that was SIGKILLed hands
    /// its keyboard back when `gone` removes this entry — nobody has to write the cleanup, and a
    /// node whose one writer died is not permanently read-only.
    /// `Arc` so a keystroke can be written **outside** this map's lock. `PtyMaster::write_all`
    /// spins on `EAGAIN` — a full tty input buffer is backpressure from a harness that has not read
    /// yet, not a failure — so a write can take arbitrarily long, and holding the map across it
    /// would let one unread node stall every other client's attach. The lease still releases the
    /// node's writer slot when the last `Arc` drops, which is the entry leaving this map.
    pub(super) leases: HashMap<ConnId, Vec<(AgentId, Arc<crate::pty::WriteLease>)>>,
    native_launches: Option<Arc<crate::native_bootstrap::PendingNativeLaunches>>,
    host_generations: HashMap<AgentId, u64>,
    next_host_generation: u64,
    pub(super) completed_count: usize,
    pub(super) completed_bytes: usize,
    pub(super) next_completed_order: u64,
    pub(super) next_completed_expiry: Option<std::time::Instant>,
    #[cfg(test)]
    completed_limit: Option<usize>,
    #[cfg(test)]
    completed_scan_count: usize,
}

/// A native ticket and writer slot validated together before the claim ACK.
///
/// The authority guard keeps ordinary attaches behind the pending gate, while `lease` reserves the
/// host's one writer slot. Neither requires holding `Panes` during socket I/O. Dropping this value
/// before commit restores the ticket and releases the slot, which makes any fallible relay
/// preparation (including lifecycle-thread creation) retryable. Commit is itself the final
/// fallible preparation step; a later acknowledgement failure rolls the whole launch back through
/// its prepared lifecycle rather than trying to resurrect a ticket whose writer was published.
pub(crate) struct PreparedNativeWriter {
    handle: Arc<RegistryHandle>,
    authority: Arc<crate::native_bootstrap::PendingNativeLaunches>,
    claim: Option<crate::native_bootstrap::PreparedNativeLaunchClaim>,
    agent_id: AgentId,
    host: Arc<crate::pty::PtyHost>,
    host_generation: u64,
    conn: ConnId,
    lease: Option<Arc<crate::pty::WriteLease>>,
}

impl PreparedNativeWriter {
    pub(crate) fn commit(
        mut self,
    ) -> Result<
        crate::native_bootstrap::NativeLaunchClaim,
        crate::native_bootstrap::NativeLaunchClaimError,
    > {
        let mut panes = lock(&self.handle.panes);
        let exact_host = panes
            .hosts
            .get(&self.agent_id)
            .is_some_and(|entry| entry.is_live() && Arc::ptr_eq(entry.host(), &self.host));
        let exact_generation =
            panes.host_generations.get(&self.agent_id) == Some(&self.host_generation);
        let exact_authority = panes
            .native_launches
            .as_ref()
            .is_some_and(|authority| Arc::ptr_eq(authority, &self.authority));
        if !exact_host
            || !exact_generation
            || !exact_authority
            || self.host.writer() != Some(self.conn)
        {
            return Err(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding);
        }
        let claim = self
            .claim
            .take()
            .expect("a prepared native writer commits once")
            .commit()?;
        panes.leases.entry(self.conn).or_default().push((
            self.agent_id.clone(),
            self.lease
                .take()
                .expect("a prepared native writer owns its lease"),
        ));
        Ok(claim)
    }
}

pub(super) enum PaneEntry {
    Live(Arc<crate::pty::PtyHost>),
    Closing(Arc<crate::pty::PtyHost>),
    Completed {
        host: Arc<crate::pty::PtyHost>,
        charged_bytes: usize,
        completed_at: std::time::Instant,
        order: u64,
    },
    Replacing(Arc<crate::pty::PtyHost>),
}

/// **Why a client may or may not type into the pane it just attached to.** The three answers are
/// deliberately one type: `writable: false` alone has meant both "somebody else is typing into
/// this running node" and "this node has finished", and a client cannot act correctly on the pair
/// collapsed into one bit. A native relay that reads a finished pane as a stolen lease abandons
/// the replay it attached to carry (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneWriteHalf {
    /// This connection holds the write half.
    Held,
    /// The pane is live and its write half belongs to another connection, named where the host
    /// still knows it.
    Elsewhere(Option<u64>),
    /// The pane generation has ended. Nobody holds its write half and nobody can be given it.
    Ended,
}

impl PaneWriteHalf {
    /// Write this answer onto the attach result. The one place the three states become wire
    /// fields, so a caller cannot spell a busy writer and an ended pane the same way.
    fn describe(self, pane: &mut marion_core::proto::result::PaneAttach) {
        let (writable, held_by, ended) = match self {
            Self::Held => (true, None, false),
            Self::Elsewhere(owner) => (false, owner, false),
            Self::Ended => (false, None, true),
        };
        pane.writable = writable;
        pane.held_by = held_by;
        pane.ended = ended;
    }
}

impl PaneEntry {
    pub(super) fn host(&self) -> &Arc<crate::pty::PtyHost> {
        match self {
            Self::Live(host) | Self::Closing(host) | Self::Replacing(host) => host,
            Self::Completed { host, .. } => host,
        }
    }

    pub(super) fn live_host(&self) -> Option<&Arc<crate::pty::PtyHost>> {
        match self {
            Self::Live(host) => Some(host),
            Self::Closing(_) | Self::Completed { .. } | Self::Replacing(_) => None,
        }
    }

    pub(super) fn replay_host(&self) -> Option<&Arc<crate::pty::PtyHost>> {
        match self {
            Self::Live(host) | Self::Closing(host) | Self::Completed { host, .. } => Some(host),
            Self::Replacing(_) => None,
        }
    }

    fn is_live(&self) -> bool {
        matches!(self, Self::Live(_))
    }
}

impl Panes {
    const MAX_COMPLETED: usize = 64;
    /// Cap on conservative cache-owned replay charge, not literal allocator/process RSS.
    pub(super) const MAX_COMPLETED_BYTES: usize = 256 * 1024 * 1024;
    const COMPLETED_TTL: std::time::Duration = std::time::Duration::from_secs(300);

    pub(super) fn completed_limit(&self) -> usize {
        #[cfg(test)]
        if let Some(limit) = self.completed_limit {
            return limit;
        }
        Self::MAX_COMPLETED
    }

    fn debit_completed(&mut self, entry: &PaneEntry) {
        let PaneEntry::Completed { charged_bytes, .. } = entry else {
            return;
        };
        self.completed_count = self
            .completed_count
            .checked_sub(1)
            .expect("every Completed insertion is debited exactly once");
        self.completed_bytes = self
            .completed_bytes
            .checked_sub(*charged_bytes)
            .expect("completed bytes are debited by their exact stored charge");
    }

    fn recompute_next_completed_expiry(&mut self) {
        self.next_completed_expiry = self
            .hosts
            .values()
            .filter_map(|entry| match entry {
                PaneEntry::Completed { completed_at, .. } => {
                    completed_at.checked_add(Self::COMPLETED_TTL)
                }
                _ => None,
            })
            .min();
    }

    fn remove_accounted(&mut self, id: &AgentId) -> Option<PaneEntry> {
        let entry = self.hosts.remove(id)?;
        self.debit_completed(&entry);
        Some(entry)
    }

    fn remove(&mut self, id: &AgentId) -> Option<PaneEntry> {
        let entry = self.remove_accounted(id)?;
        if matches!(entry, PaneEntry::Completed { .. }) {
            self.recompute_next_completed_expiry();
        }
        Some(entry)
    }

    fn replace(&mut self, id: AgentId, entry: PaneEntry) -> Option<PaneEntry> {
        let old = self.hosts.insert(id, entry);
        if let Some(old) = old.as_ref() {
            self.debit_completed(old);
        }
        self.recompute_next_completed_expiry();
        old
    }

    fn complete(
        &mut self,
        id: &AgentId,
        host: &Arc<crate::pty::PtyHost>,
        charged_bytes: usize,
        completed_at: std::time::Instant,
    ) -> bool {
        if !matches!(self.hosts.get(id), Some(PaneEntry::Closing(current)) if Arc::ptr_eq(current, host))
        {
            return false;
        }
        // A candidate that cannot fit by itself must not evict valid older completions merely to
        // discover that it still cannot fit.
        if charged_bytes > Self::MAX_COMPLETED_BYTES {
            return false;
        }
        let Some(completed_count) = self.completed_count.checked_add(1) else {
            return false;
        };
        let Some(completed_bytes) = self.completed_bytes.checked_add(charged_bytes) else {
            return false;
        };
        let Some(next_order) = self.next_completed_order.checked_add(1) else {
            return false;
        };
        let order = self.next_completed_order;
        self.next_completed_order = next_order;
        self.completed_count = completed_count;
        self.completed_bytes = completed_bytes;
        self.hosts.insert(
            id.clone(),
            PaneEntry::Completed {
                host: Arc::clone(host),
                charged_bytes,
                completed_at,
                order,
            },
        );
        if let Some(expiry) = completed_at.checked_add(Self::COMPLETED_TTL)
            && self
                .next_completed_expiry
                .is_none_or(|next_expiry| expiry < next_expiry)
        {
            self.next_completed_expiry = Some(expiry);
        }
        true
    }

    fn take_completed_victims(&mut self, now: std::time::Instant) -> Vec<Arc<crate::pty::PtyHost>> {
        if self.completed_count == 0 {
            return Vec::new();
        }
        if self.completed_within_budget(now) {
            return Vec::new();
        }
        #[cfg(test)]
        {
            self.completed_scan_count += 1;
        }
        let mut victims = self.take_expired_completed(now);
        self.take_oldest_completed_over_budget(&mut victims);
        self.recompute_next_completed_expiry();
        victims
    }

    /// **Nothing is past its deadline and nothing is over budget**, so no Completed pane is a
    /// victim and the scan below is skipped entirely.
    fn completed_within_budget(&self, now: std::time::Instant) -> bool {
        self.next_completed_expiry
            .is_some_and(|next_expiry| now < next_expiry)
            && self.completed_count <= self.completed_limit()
            && self.completed_bytes <= Self::MAX_COMPLETED_BYTES
    }

    /// Every Completed pane at or past [`Self::COMPLETED_TTL`], taken out of the accounting.
    fn take_expired_completed(&mut self, now: std::time::Instant) -> Vec<Arc<crate::pty::PtyHost>> {
        let expired = self
            .hosts
            .iter()
            .filter_map(|(id, entry)| match entry {
                PaneEntry::Completed { completed_at, .. }
                    if now.saturating_duration_since(*completed_at) >= Self::COMPLETED_TTL =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter_map(|id| self.remove_accounted(&id))
            .map(|entry| Arc::clone(entry.host()))
            .collect::<Vec<_>>()
    }

    /// **Oldest first**, until the retained Completed panes are back inside both budgets.
    fn take_oldest_completed_over_budget(&mut self, victims: &mut Vec<Arc<crate::pty::PtyHost>>) {
        while self.completed_count > self.completed_limit()
            || self.completed_bytes > Self::MAX_COMPLETED_BYTES
        {
            let Some(oldest) = self
                .hosts
                .iter()
                .filter_map(|(id, entry)| match entry {
                    PaneEntry::Completed { order, .. } => Some((id.clone(), *order)),
                    _ => None,
                })
                .min_by_key(|(_, order)| *order)
                .map(|(id, _)| id)
            else {
                break;
            };
            if let Some(entry) = self.remove_accounted(&oldest) {
                victims.push(Arc::clone(entry.host()));
            }
        }
    }

    /// `conn`'s lease on `id`, if it holds one. The lease **is** the permission — there is no
    /// separate boolean to disagree with it — so a caller that gets `None` here has been refused.
    pub(super) fn lease(&self, conn: ConnId, id: &AgentId) -> Option<Arc<crate::pty::WriteLease>> {
        self.leases
            .get(&conn)?
            .iter()
            .find(|(a, _)| a == id)
            .map(|(_, l)| Arc::clone(l))
    }

    pub(super) fn has_live(&self, id: &AgentId) -> bool {
        self.hosts.get(id).is_some_and(PaneEntry::is_live)
    }

    fn assign_host_generation(&mut self, id: &AgentId) -> Option<u64> {
        let generation = self.next_host_generation.checked_add(1)?;
        self.next_host_generation = generation;
        self.host_generations.insert(id.clone(), generation);
        Some(generation)
    }

    fn revoke_native_launch(&mut self, id: &AgentId) {
        self.host_generations.remove(id);
        if let Some(launches) = self.native_launches.as_ref() {
            launches.revoke_agent(id);
        }
    }
}

impl RegistryHandle {
    fn pane_now(&self) -> std::time::Instant {
        let clock = Arc::clone(&lock(&self.pane_clock));
        clock()
    }

    fn retire_pane_hosts(hosts: Vec<Arc<crate::pty::PtyHost>>) {
        for host in hosts {
            host.clear_legacy_listeners();
            host.invalidate_pane_streams();
        }
    }

    pub(super) fn prune_completed_panes(&self) {
        let now = self.pane_now();
        let victims = lock(&self.panes).take_completed_victims(now);
        Self::retire_pane_hosts(victims);
    }

    #[cfg(test)]
    pub(super) fn set_pane_clock_for_test(
        &self,
        clock: Arc<dyn Fn() -> std::time::Instant + Send + Sync>,
    ) {
        *lock(&self.pane_clock) = clock;
    }

    #[cfg(test)]
    pub(super) fn completed_usage_for_test(&self) -> (usize, usize) {
        let panes = lock(&self.panes);
        (panes.completed_count, panes.completed_bytes)
    }

    #[cfg(test)]
    pub(super) fn next_completed_order_for_test(&self) -> u64 {
        lock(&self.panes).next_completed_order
    }

    #[cfg(test)]
    pub(super) fn completed_scan_count_for_test(&self) -> usize {
        lock(&self.panes).completed_scan_count
    }

    #[cfg(test)]
    pub(super) fn reset_completed_scan_count_for_test(&self) {
        lock(&self.panes).completed_scan_count = 0;
    }

    #[cfg(test)]
    pub(super) fn set_completed_limit_for_test(&self, limit: usize) {
        lock(&self.panes).completed_limit = Some(limit);
    }

    /// The nodes this supervisor holds a pty for, as a snapshot.
    ///
    /// Cloned out under the panes lock and read afterwards, so no caller holds two locks at once.
    /// The window that opens — a pane created between this read and the projection — resolves at
    /// the next flush, and the alternative is the lock coupling [`Panes`] exists to avoid.
    pub(super) fn pane_ids(&self) -> HashSet<AgentId> {
        lock(&self.panes)
            .hosts
            .iter()
            .filter_map(|(id, entry)| entry.is_live().then_some(id.clone()))
            .collect()
    }

    /// §2's `node/attach` — §7.3.3's re-attach, **both legs and no seam between them**.
    ///
    /// `events.rs` argues the shape and this is where it is spent: replay and subscribe are one
    /// [`EventReader`](crate::events::EventReader) cursor over one file, so *"replay to the journal's own read point, then
    /// subscribe from there"* is not a procedure anybody has to implement correctly — the reader
    /// returns the intact prefix and its own byte offset in a single read, this method sends that
    /// prefix, and every later poll continues from that same offset. **A gap or a duplicate at the
    /// join is unreachable because there is no join.**
    ///
    /// So this needs none of the locking [`Self::subscribe`] needs, and that difference is
    /// structural rather than an omission. `tree/subscribe` derives *two* things — a snapshot and
    /// the told-set that decides what the next notification will be — and must derive them from one
    /// view or a notification can slip between them. An attach derives one: the reader's own read.
    ///
    /// **The replayed events go out as `node/event` notifications, before the response frame.**
    /// [`marion_core::proto::result::NodeAttachResult`] has no field for them and deliberately so: a
    /// replayed event and a live one are the same event from the same file, and giving the replay a
    /// second shape on the wire would ask every client to write the splice this module exists to
    /// make impossible. Because they precede the response, the [`ReplayPoint`] in the answer is a
    /// statement in the past tense — *everything up to here has already been sent to you* — which
    /// is stronger than a promise about what will arrive.
    ///
    /// **The cursor is kept even for a node that has already exited**, though the mode says the
    /// node is finished. The two are not in tension: the mode describes the *node*, the cursor
    /// describes this reader. A node marion has just journaled as `Exited` may still have its
    /// `Lifecycle::Exited` bookend in flight to its own `events.jsonl` — different files, different
    /// writers, no ordering between them — and dropping the cursor on the strength of the journal
    /// would lose precisely the record that separates a node that finished from one cut mid-turn.
    /// A cursor on a file nobody will append to costs one `stat` per tick and delivers nothing.
    pub(super) fn node_attach(
        &self,
        id: &AgentId,
        pane_stream_v1: bool,
        out: &Outbound,
    ) -> Result<NodeAttachResult, RpcError> {
        let (mut summary, state, reap_state, project) = self.live.read(|r| {
            let Some(node) = r.tree().get(id) else {
                return Err(RpcError::not_found(
                    &id.0,
                    format!(
                        "this project's journal records no node `{}`, so there is no stream to \
                         attach to. The registry is current as of {} records read; a node spawned \
                         by another process appears here once its `SpawnIntent` is on disk \
                         (§6.1 step 7).",
                        id.0,
                        r.read_point().records
                    ),
                    "§3.2",
                ));
            };
            let summary = summarize(node, false).map_err(|e| e.as_error(id))?;
            Ok((summary, node.state, node.reap_state, r.project()))
        })?;

        let Some(project) = project else {
            return Err(RpcError::internal(format!(
                "the supervisor cannot work out which project directory node `{}`'s stream lives \
                 under: its registry was booted over a journal path with no `<state>/<hash>/` \
                 above it. This is a misconfigured supervisor, not a missing node.",
                id.0
            )));
        };
        let events_path = project.agent(id).events();
        let (reader, replayed) = crate::events::EventReader::open_path(&events_path)
            .map_err(|e| attach_io_failure(id, &events_path, &e))?;

        // **"Nobody recorded this" is not "it said nothing", and a terminal node is where the two
        // stop being distinguishable by waiting.** `EventReader::ever_written` is the split, and
        // §4.1's whole vocabulary exists so marion does not report an unrecorded node as an empty
        // transcript. For a node that is still running the answer is to attach anyway — the file
        // appears on its first frame and the reader is already watching for it. For one that has
        // exited, no frame is coming, and `ReplayOnly(records: 0)` would be marion asserting it
        // observed silence it never observed.
        if !reader.ever_written() && state.is_exited() {
            return Err(RpcError::of(
                FailureKind::NotFound,
                Some(&id.0),
                format!(
                    "node `{}` has exited and marion has no `{}` for it, so there is nothing to \
                     replay and nothing more will be written. This is **not** an empty transcript: \
                     it is a node whose stream was never recorded — the recorder could not open the \
                     file, or this node ran under a build that did not record one (§4.1). Reporting \
                     it as a node that said nothing would be a claim marion cannot make.",
                    id.0,
                    events_path.display()
                ),
                "§7.3.3",
            ));
        }

        let point = reader.read_point();
        let mode = attach_mode(state, reap_state, point);
        // A versioned pane reservation is the first side effect. If its bounded cursor, entropy,
        // or retained generation is unavailable, no NodeEvent, cursor, listener, or lease has
        // moved. It must never silently downgrade to the lossy legacy stream.
        let (mut reserved_pane, reserved_host) = if pane_stream_v1 {
            self.attach_pane_v1(id, out)?
        } else {
            (None, None)
        };
        // Sent before the reader is parked, so nothing appended between the two can be delivered
        // ahead of the replay it comes after.
        let live = deliver_events(out, id, &replayed);
        if !live {
            if let Some(host) = reserved_host {
                self.rollback_pane_attach(id, out.conn(), &host);
            }
            return Err(RpcError::internal(format!(
                "node `{}`'s attach connection closed while its durable event prefix was queued; \
                 the pane reservation and write lease were rolled back",
                id.0
            )));
        }
        #[cfg(test)]
        if let Some(hook) = lock(&self.pane_attach_selection_hook).take() {
            hook();
        }
        if pane_stream_v1 {
            self.commit_pane_v1_reservation(id, out, &mut reserved_pane, reserved_host.as_ref())?;
        }
        let watch = AttachWatch::start(reader.path(), self.live.changes());
        lock(&self.shared).attached.push(Attachment {
            conn: out.conn(),
            agent_id: id.clone(),
            reader,
            out: out.clone(),
            watch,
        });
        // The loop's next pass reads the new cursor, and learns whether it has a deadline now.
        self.live.changes().notify();
        // Legacy keeps its historical NodeEvent-before-NodePty ordering. V1 already reserved a
        // paced cursor, but sends no pane frame until the response has advertised its exact token.
        let pane = if pane_stream_v1 {
            reserved_pane
        } else {
            self.attach_pane(id, out)
        };
        summary.pane = pane.is_some();
        Ok(NodeAttachResult {
            node: summary,
            mode,
            pane,
        })
    }

    /// **One look at the pane this attach reserved**, under the registry lock: is the
    /// retained generation still the current one, and is this connection its writer?
    fn pane_reservation_snapshot(
        &self,
        id: &AgentId,
        conn: ConnId,
        host: &Arc<crate::pty::PtyHost>,
        descriptor: &marion_core::proto::result::PaneReadyDescriptorV1,
    ) -> (bool, PaneWriteHalf) {
        let panes = lock(&self.panes);
        let (same_generation, write_half) = match panes.hosts.get(id) {
            Some(PaneEntry::Live(current)) if Arc::ptr_eq(current, host) => {
                let writable = panes.lease(conn, id).is_some();
                (
                    true,
                    if writable {
                        PaneWriteHalf::Held
                    } else {
                        PaneWriteHalf::Elsewhere(host.writer().map(|owner| owner.0))
                    },
                )
            }
            Some(PaneEntry::Closing(current)) if Arc::ptr_eq(current, host) => {
                (true, PaneWriteHalf::Ended)
            }
            Some(PaneEntry::Completed { host: current, .. }) if Arc::ptr_eq(current, host) => {
                (true, PaneWriteHalf::Ended)
            }
            _ => (false, PaneWriteHalf::Ended),
        };
        // Panes remains held through exact Pending validation. Closing therefore
        // linearizes wholly before this snapshot (read-only) or wholly after the attach
        // commit; it cannot revoke the lease between metadata and token validation.
        let exact_pending =
            same_generation && host.pane_replay_reserved(conn, &descriptor.token, descriptor.cut);
        (exact_pending, write_half)
    }

    /// **The versioned reservation's commit point**: the pane a v1 attach reserved is
    /// still the current generation and still exactly Pending, or the reservation is
    /// rolled back and the attach refused as a conflict.
    fn commit_pane_v1_reservation(
        &self,
        id: &AgentId,
        out: &Outbound,
        reserved_pane: &mut Option<marion_core::proto::result::PaneAttach>,
        reserved_host: Option<&Arc<crate::pty::PtyHost>>,
    ) -> Result<(), RpcError> {
        if let (Some(pane), Some(host)) = (reserved_pane.as_mut(), reserved_host) {
            let descriptor = pane
                .pane_ready
                .as_ref()
                .expect("a versioned reservation carries its Ready descriptor");
            let (valid_reservation, write_half) =
                self.pane_reservation_snapshot(id, out.conn(), host, descriptor);
            if !valid_reservation {
                self.rollback_pane_attach(id, out.conn(), host);
                return Err(RpcError::conflict(
                    &id.0,
                    format!(
                        "node `{}`'s pane generation changed while attach was being prepared; \
                         retry against the current generation",
                        id.0
                    ),
                    "§5.3",
                ));
            }
            write_half.describe(pane);
        }
        Ok(())
    }

    /// Claim the write half for `conn` on a **live** host, or say who has it. Re-attaching from
    /// the connection that already holds it answers `Held` with the lease it already had rather
    /// than issuing a second one: two live leases for one connection would each clear the slot on
    /// drop, and the first drop would silently open the node to a third client.
    fn take_write_half(
        panes: &mut Panes,
        host: &Arc<crate::pty::PtyHost>,
        id: &AgentId,
        conn: ConnId,
    ) -> PaneWriteHalf {
        if panes.lease(conn, id).is_some() {
            return PaneWriteHalf::Held;
        }
        match host.lease_writer(conn) {
            Ok(lease) => {
                panes
                    .leases
                    .entry(conn)
                    .or_default()
                    .push((id.clone(), Arc::new(lease)));
                PaneWriteHalf::Held
            }
            Err(crate::pty::WriterBusy::HeldBy(owner)) => PaneWriteHalf::Elsewhere(Some(owner.0)),
        }
    }

    /// The **display plane's** half of `node/attach` (§5.3), or `None` for a node with no pty.
    ///
    /// Three things happen here and they are deliberately one step, in this order:
    ///
    /// 1. **Listen first.** Registering the byte fan-out before claiming the keyboard means the
    ///    client that *is* refused the write half still sees the node — a read-only attach is the
    ///    point of the refusal, not a consolation for it. Doing it the other way round would leave
    ///    a window in which this client could type and not yet see the echo.
    /// 2. **Claim the write half, once.** `lease_writer` refuses the second caller by name; the
    ///    lease is parked under this connection so it is released by `gone` dropping it, which is
    ///    what makes a crashed client's node writable again with no cleanup code anywhere.
    /// 3. **Report the pty's current size**, because it is not the client's: the master was sized
    ///    before the child existed, by a supervisor that could not know what terminal would
    ///    eventually attach. A client that rendered without asking would render somebody else's
    ///    geometry.
    ///
    /// **Re-attaching from the same connection does not re-lease.** `lease_writer` refuses a
    /// connection that already holds the slot rather than issuing a second lease, so the answer to
    /// a second `node/attach` from a writer is `writable: true` with the lease it already had —
    /// two live leases for one connection would each clear the slot on drop, and the first drop
    /// would silently open the node to a third client.
    pub(super) fn attach_pane(
        &self,
        id: &AgentId,
        out: &Outbound,
    ) -> Option<marion_core::proto::result::PaneAttach> {
        self.prune_completed_panes();
        // Held through listener and lease commit. `forget_pane` cannot invalidate a host between
        // those two halves, and neither host operation performs a delivery callback while this
        // global registry lock is held.
        let mut panes = lock(&self.panes);
        let host = panes.hosts.get(id)?.live_host().cloned()?;
        host.unlisten(out.conn());
        host.listen(out.clone());
        #[cfg(test)]
        if let Some(hook) = lock(&self.pane_listener_hook).take() {
            hook();
        }
        // The size the master really has, not the size it was asked for: `PtyMaster::size` reads
        // `TIOCGWINSZ` back, so a client is told what the child will see rather than what marion
        // intended it to see.
        let size = host
            .master()
            .size()
            .unwrap_or_else(|_| host.master().intended_size());
        let native_reserved = panes
            .native_launches
            .as_ref()
            .is_some_and(|launches| launches.has_pending(id));
        // A pending native launch is a write half already promised to a claimant that has not
        // arrived: read-only, but the node is running, so it is `Elsewhere` and not `Ended`.
        let write_half = if native_reserved {
            PaneWriteHalf::Elsewhere(None)
        } else {
            Self::take_write_half(&mut panes, &host, id, out.conn())
        };
        let mut pane = marion_core::proto::result::PaneAttach {
            cols: size.cols,
            rows: size.rows,
            writable: false,
            held_by: None,
            ended: false,
            pane_ready: None,
        };
        write_half.describe(&mut pane);
        Some(pane)
    }

    pub(super) fn attach_pane_v1(
        &self,
        id: &AgentId,
        out: &Outbound,
    ) -> Result<
        (
            Option<marion_core::proto::result::PaneAttach>,
            Option<Arc<crate::pty::PtyHost>>,
        ),
        RpcError,
    > {
        self.prune_completed_panes();
        let mut panes = lock(&self.panes);
        let Some(entry) = panes.hosts.get(id) else {
            return Ok((None, None));
        };
        let Some(host) = entry.replay_host().cloned() else {
            return Err(RpcError::refused(
                &id.0,
                format!(
                    "node `{}`'s pane generation is being replaced; retry attach",
                    id.0
                ),
                "§5.3",
            ));
        };
        let is_live = entry.is_live();
        let descriptor = host
            .reserve_pane_replay(out.conn(), out.clone())
            .map_err(|error| match error {
                crate::pty::PaneReplayReservationError::Busy => RpcError::refused(
                    &id.0,
                    format!(
                        "node `{}` could not reserve another bounded pane replay: {error}",
                        id.0
                    ),
                    "§5.3",
                ),
                crate::pty::PaneReplayReservationError::Unavailable(_)
                | crate::pty::PaneReplayReservationError::Entropy => RpcError::internal(format!(
                    "node `{}` could not create an exact pane replay: {error}",
                    id.0
                )),
            })?;
        let native_reserved = panes
            .native_launches
            .as_ref()
            .is_some_and(|launches| launches.has_pending(id));
        let write_half = if !is_live {
            PaneWriteHalf::Ended
        } else if native_reserved {
            PaneWriteHalf::Elsewhere(None)
        } else {
            Self::take_write_half(&mut panes, &host, id, out.conn())
        };
        let size = host
            .master()
            .size()
            .unwrap_or_else(|_| host.master().intended_size());
        let mut pane = marion_core::proto::result::PaneAttach {
            cols: size.cols,
            rows: size.rows,
            writable: false,
            held_by: None,
            ended: false,
            pane_ready: Some(descriptor),
        };
        write_half.describe(&mut pane);
        Ok((Some(pane), Some(host)))
    }

    fn rollback_pane_attach(&self, id: &AgentId, conn: ConnId, host: &Arc<crate::pty::PtyHost>) {
        {
            let mut panes = lock(&self.panes);
            if panes
                .hosts
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(entry.host(), host))
                && let Some(leases) = panes.leases.get_mut(&conn)
            {
                leases.retain(|(agent_id, _)| agent_id != id);
                if leases.is_empty() {
                    panes.leases.remove(&conn);
                }
            }
        }
        host.unlisten(conn);
    }

    /// Register a node's pty with this supervisor, so `node/attach` can find it.
    ///
    /// The one route in. A `PtyHost` that is never registered is a recording nobody can watch, and
    /// a pane registered for a node the journal does not know about is unattachable — `node_attach`
    /// resolves the node from the registry *before* it looks here, so the journal stays the
    /// authority on what exists.
    ///
    /// A newly published terminal is also where turn delivery starts for a row that takes turns
    /// by pasting ([`Self::attach_paste_delivery`]): a pane and a native session both arrive here,
    /// so one call covers both, and a replacement host gets a fresh injector.
    pub fn register_pane(&self, id: &AgentId, host: Arc<crate::pty::PtyHost>) {
        if self.publish_pane(id, Arc::clone(&host)) {
            self.attach_paste_delivery(id, &host);
        }
    }

    /// `register_pane`'s table half: `true` when `host` became the node's live pane, `false` when
    /// it already was (idempotent) or a newer registration superseded it mid-replacement.
    fn publish_pane(&self, id: &AgentId, host: Arc<crate::pty::PtyHost>) -> bool {
        self.prune_completed_panes();
        let _replacement = lock(&self.pane_replacement);
        let replacement = Arc::clone(&host);
        let (old, revoked_leases) = {
            let mut panes = lock(&self.panes);
            if let Some(existing) = panes.hosts.get(id) {
                if Arc::ptr_eq(existing.host(), &replacement) {
                    // Live is idempotent; lifecycle states never move backwards through register.
                    return false;
                }
            } else {
                panes.hosts.insert(id.clone(), PaneEntry::Live(host));
                let _ = panes.assign_host_generation(id);
                return true;
            }
            panes.revoke_native_launch(id);
            let old = panes.replace(id.clone(), PaneEntry::Replacing(Arc::clone(&replacement)));
            let mut revoked = Vec::new();
            for leases in panes.leases.values_mut() {
                let mut kept = Vec::with_capacity(leases.len());
                for (agent_id, lease) in std::mem::take(leases) {
                    if &agent_id == id {
                        revoked.push(lease);
                    } else {
                        kept.push((agent_id, lease));
                    }
                }
                *leases = kept;
            }
            panes.leases.retain(|_, leases| !leases.is_empty());
            (old, revoked)
        };
        drop(revoked_leases);
        let old = old.expect("an existing generation was replaced above");
        let old_host = Arc::clone(old.host());
        old_host.clear_legacy_listeners();
        old_host.invalidate_pane_streams();
        drop(old);

        let published = {
            let mut panes = lock(&self.panes);
            let matching = panes.hosts.get(id).is_some_and(
                |entry| matches!(entry, PaneEntry::Replacing(current) if Arc::ptr_eq(current, &replacement)),
            );
            if matching {
                panes.replace(id.clone(), PaneEntry::Live(Arc::clone(&replacement)));
                let _ = panes.assign_host_generation(id);
            }
            matching
        };
        if !published {
            replacement.clear_legacy_listeners();
            replacement.invalidate_pane_streams();
        }
        published
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn install_pending_native_launches(
        &self,
        launches: Arc<crate::native_bootstrap::PendingNativeLaunches>,
    ) -> Result<(), Arc<crate::native_bootstrap::PendingNativeLaunches>> {
        let mut panes = lock(&self.panes);
        if panes.native_launches.is_some() {
            return Err(launches);
        }
        panes.native_launches = Some(launches);
        Ok(())
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn reserve_pending_native_launch(
        self: &Arc<Self>,
        binding: crate::native_bootstrap::NativeLaunchBinding,
    ) -> Result<
        crate::native_bootstrap::PendingNativeLaunchReceipt,
        crate::native_bootstrap::NativeLaunchReservationError,
    > {
        let panes = lock(&self.panes);
        if panes.hosts.contains_key(binding.agent_id()) {
            return Err(crate::native_bootstrap::NativeLaunchReservationError::HostAlreadyVisible);
        }
        let launches = panes
            .native_launches
            .as_ref()
            .ok_or(crate::native_bootstrap::NativeLaunchReservationError::UnknownTicket)?;
        launches.reserve(binding)
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn publish_pending_native_launch(
        &self,
        receipt: &crate::native_bootstrap::NativeLaunchReceipt,
    ) -> Result<u64, crate::native_bootstrap::NativeLaunchReservationError> {
        let panes = lock(&self.panes);
        if !matches!(
            panes.hosts.get(receipt.agent_id()),
            Some(PaneEntry::Live(_))
        ) {
            return Err(crate::native_bootstrap::NativeLaunchReservationError::HostUnavailable);
        }
        let generation = panes
            .host_generations
            .get(receipt.agent_id())
            .copied()
            .ok_or(crate::native_bootstrap::NativeLaunchReservationError::HostUnavailable)?;
        let launches = panes
            .native_launches
            .as_ref()
            .ok_or(crate::native_bootstrap::NativeLaunchReservationError::UnknownTicket)?;
        launches.publish(receipt, generation)?;
        Ok(generation)
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn claim_pending_native_writer(
        &self,
        ticket: &crate::native_bootstrap::NativeLaunchTicket,
        agent_id: &AgentId,
        claimant: crate::native_bootstrap::NativeClaimant,
    ) -> Result<
        crate::native_bootstrap::NativeLaunchClaim,
        crate::native_bootstrap::NativeLaunchClaimError,
    > {
        // Dark internal seam only: `NativeClaimant` has no production issuer and no public wire
        // vocabulary carries a launch ticket. Production NativeBootstrapService remains disabled.
        let mut panes = lock(&self.panes);
        let host = panes
            .hosts
            .get(agent_id)
            .and_then(PaneEntry::live_host)
            .cloned()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)?;
        let host_generation = panes
            .host_generations
            .get(agent_id)
            .copied()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)?;
        let launches = panes
            .native_launches
            .as_ref()
            .cloned()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::UnknownTicket)?;
        // Panes stays held from exact ticket consumption through writer publication. An ordinary
        // attach therefore sees either the pending gate or the completed lease, never the seam.
        let conn = claimant.conn();
        let (claim, lease) =
            launches.claim_with(ticket, agent_id, host_generation, claimant, || {
                host.lease_writer(conn)
                    .map_err(|_| crate::native_bootstrap::NativeLaunchClaimError::WriterBusy)
            })?;
        panes
            .leases
            .entry(conn)
            .or_default()
            .push((agent_id.clone(), Arc::new(lease)));
        Ok(claim)
    }

    /// Validate a same-socket native claim and reserve its exact writer slot without consuming the
    /// ticket. The returned guard performs the final, non-I/O writer commit only after every other
    /// fallible relay resource is ready and before the service delivers the claim acknowledgement;
    /// post-acknowledgement relay start is therefore infallible.
    pub(crate) fn prepare_pending_native_writer(
        self: &Arc<Self>,
        ticket: &crate::native_bootstrap::NativeLaunchTicket,
        agent_id: &AgentId,
        claimant: crate::native_bootstrap::NativeClaimant,
    ) -> Result<PreparedNativeWriter, crate::native_bootstrap::NativeLaunchClaimError> {
        let panes = lock(&self.panes);
        let host = panes
            .hosts
            .get(agent_id)
            .and_then(PaneEntry::live_host)
            .cloned()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)?;
        let host_generation = panes
            .host_generations
            .get(agent_id)
            .copied()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)?;
        let authority = panes
            .native_launches
            .as_ref()
            .cloned()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::UnknownTicket)?;
        let claim = authority.prepare_claim(ticket, agent_id, host_generation, claimant)?;
        let conn = claimant.conn();
        let lease = host
            .lease_writer(conn)
            .map_err(|_| crate::native_bootstrap::NativeLaunchClaimError::WriterBusy)?;
        Ok(PreparedNativeWriter {
            handle: Arc::clone(self),
            authority,
            claim: Some(claim),
            agent_id: agent_id.clone(),
            host,
            host_generation,
            conn,
            lease: Some(Arc::new(lease)),
        })
    }

    /// Stop admitting new live-pane operations while keeping the exact host registered through
    /// reader drain. A stale close callback cannot transition a replacement host with the same id.
    pub fn closing_pane(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        // Serialize with same-id publication. A replacement briefly occupies `Replacing(new)`
        // while the old generation's admitted legacy deliveries drain; a fast new child may exit
        // in that interval. Waiting here ensures its Closing transition happens after publication
        // instead of being discarded and later overwritten by Live.
        let _replacement = lock(&self.pane_replacement);
        let mut panes = lock(&self.panes);
        if !matches!(panes.hosts.get(id), Some(PaneEntry::Live(current)) if Arc::ptr_eq(current, host))
        {
            return;
        }
        panes.revoke_native_launch(id);
        panes
            .hosts
            .insert(id.clone(), PaneEntry::Closing(Arc::clone(host)));
        host.seal_controls();
        for leases in panes.leases.values_mut() {
            leases.retain(|(agent_id, _)| agent_id != id);
        }
    }

    /// Publish a drained host as read-only replay state. Only the matching closing generation can
    /// complete, so an old launcher cannot overwrite a newly registered pane with the same id.
    pub fn completed_pane(
        &self,
        id: &AgentId,
        host: &Arc<crate::pty::PtyHost>,
        charged_bytes: usize,
    ) {
        let now = self.pane_now();
        let (completed, victims) = {
            let mut panes = lock(&self.panes);
            let matching = matches!(panes.hosts.get(id), Some(PaneEntry::Closing(current)) if Arc::ptr_eq(current, host));
            if !matching {
                return;
            }
            let completed = panes.complete(id, host, charged_bytes, now);
            let mut victims = panes.take_completed_victims(now);
            if !completed && let Some(refused) = panes.remove(id) {
                victims.push(Arc::clone(refused.host()));
            }
            (completed, victims)
        };
        if completed {
            host.clear_legacy_listeners();
        }
        Self::retire_pane_hosts(victims);
    }

    /// Permanently invalidate one failed host generation without touching a replacement that now
    /// owns the same agent id.
    pub fn failed_pane(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        let removed = {
            let mut panes = lock(&self.panes);
            let matching = panes
                .hosts
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(entry.host(), host));
            if matching {
                panes.revoke_native_launch(id);
                for leases in panes.leases.values_mut() {
                    leases.retain(|(agent_id, _)| agent_id != id);
                }
                panes.remove(id)
            } else {
                None
            }
        };
        host.clear_legacy_listeners();
        host.invalidate_pane_streams();
        drop(removed);
    }

    /// Forget a node's pty. Called when the node ends: the host is dropped by its owner, and
    /// leaving a dangling entry here would answer a later `node/attach` with a pane onto a master
    /// that has already been closed.
    pub fn forget_pane(&self, id: &AgentId) {
        let host = {
            let mut panes = lock(&self.panes);
            panes.revoke_native_launch(id);
            let host = panes.remove(id).map(|entry| Arc::clone(entry.host()));
            for leases in panes.leases.values_mut() {
                leases.retain(|(a, _)| a != id);
            }
            host
        };
        if let Some(host) = host {
            host.clear_legacy_listeners();
            host.invalidate_pane_streams();
        }
    }

    /// How many nodes this supervisor holds a pty for. Diagnostic, and the assertion surface for
    /// `register_pane`/`forget_pane`.
    pub fn panes(&self) -> usize {
        lock(&self.panes).hosts.len()
    }

    /// **A contracted child in a pane could only time out**: `pane` on a spawn with a `caller`.
    pub(super) fn check_child_pane(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        // **The third field on the same rule, refused for a reason of its own.** Not symmetry with
        // the two above: a pane is a TUI, and a TUI takes no turn until a human presses return,
        // while a child is defined by a `TaskContract` it must `report` against inside a wall
        // clock. A contracted child in a pane is therefore a task that can only ever time out, and
        // its contract would record that as the child's failure rather than as marion's category
        // error. `run::run_spawn` refuses `LaunchPath::Terminal` again one layer down; this is the
        // refusal in the frame that asked for it.
        if p.caller.is_some() && p.pane.is_some() {
            return Err(RpcError::refused(
                "pane",
                "a spawn with a `caller` must not state `pane`: a pane is a terminal an operator \
                 attaches to and drives by keystrokes, and a child is defined by the \
                 `TaskContract` it must report against inside its wall clock. A TUI takes no turn \
                 until somebody presses return, so a contracted child in a pane is a task that can \
                 only time out — and the contract would record that as the child's failure. Spawn \
                 it without a pane and watch its structured events through `node/attach`, or start \
                 it as a root. Refused rather than dropped (§11 item 23).",
                "§9 M3, §11 item 23",
            ));
        }
        Ok(())
    }
}

/// §7.3.3's three answers, chosen from the two fields that decide them and nothing else.
///
/// Any non-`Live` reap state is tested **before** the exit. `ReapedIdle` because §7.3.2's
/// disposition (c) reaps a node that is idle rather than one that is finished, and a reaped node
/// whose journal also shows an exit is still the one the operator can bring back. `Orphaned`
/// (§7.2) because this supervisor booted over a record it did not write and holds no channel for
/// the node — `ResubscribeFrom` would promise live events nobody can deliver — while the record
/// itself is complete and the node is the operator's to bring back. Collapsing either into
/// `ReplayOnly` would tell a client its only option is to read, when the node is resumable.
pub(super) fn attach_mode(
    state: NodeState,
    reap_state: ReapState,
    point: ReplayPoint,
) -> AttachMode {
    if reap_state != ReapState::Live {
        AttachMode::ReplayResumable(point)
    } else if state.is_exited() {
        AttachMode::ReplayOnly(point)
    } else {
        AttachMode::ResubscribeFrom(point)
    }
}

/// A node's stream exists and could not be read — a fault in the supervisor's own storage, not
/// something the caller did, so `Internal` and not a refusal.
fn attach_io_failure(
    id: &AgentId,
    path: &std::path::Path,
    error: &crate::events::EventError,
) -> RpcError {
    RpcError::internal(format!(
        "node `{}`'s event stream at {} could not be read: {error}. A missing file is not this \
         case — that is a node marion never recorded, and it is reported as such.",
        id.0,
        path.display()
    ))
}
