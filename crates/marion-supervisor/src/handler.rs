//! The registry, answering (design §2's `node/get` and `tree/subscribe`; §7.3.3's seam).
//!
//! [`serve`](crate::serve) knows about frames and connections and nothing about nodes;
//! [`registry`](crate::registry) knows about nodes and nothing about clients. This is the seam, and
//! it is where three questions the registry deliberately refused to answer get answered — or are
//! refused again, in words, which is the other honest outcome.
//!
//! # Projecting a node is fallible, and that is the finding
//!
//! `registry.rs` states that it will not build a [`NodeSummary`] because two of its fields —
//! `timeout` and `name` — *"have no source in the journal at all"*, and that building one means
//! fabricating both. Neither is fabricated here:
//!
//! * **`timeout`** is resolved from the node's recorded `agent_type` through
//!   [`marion_core::agent_type::builtin`] — the same registry that decided the bound when the node
//!   was spawned. §3.1 makes the agent type the source of truth for it and §9 *"re-resolves it fresh
//!   on resume"*, so reading it from the type is what the spec already prescribes; a journalled copy
//!   would be the second source of truth, not the first.
//! * **`name`** is `None`, which is not a placeholder but the truth. §2's `node/rename` is the only
//!   thing that sets `Node.name`, it does not exist, and nothing has ever written one. When it lands
//!   it lands as a journal record and is read here; until then `None` is what the journal says.
//! * **`agent_type`, `harness`, `depth`** come from the `SpawnIntent`, which `marion-core` makes an
//!   `Option` precisely because a journal whose head was compacted away, or whose intent record was
//!   lost, has records *about* a node and no identity *for* it.
//!
//! So a node can be **unprojectable**, and [`summarize`] returns that rather than inventing a
//! summary. Three ways, each named: no intent at all, an `agent_type` this build does not know, and
//! a depth that will not fit `NodeSummary`'s `u8`. A `node/get` for such a node is a refusal that
//! says which fact is missing, and — see below — a `tree/subscribe` counts them rather than dropping
//! them into silence.
//!
//! # The snapshot and the subscription begin at the same instant
//!
//! §7.3.3 makes the replay-to-subscribe seam *the* correctness question, and
//! [`marion_proto::result::TreeSubscribeResult`] says why the two travel together: *"a client that
//! renders a tree and **then** starts listening has a window it cannot account for."*
//!
//! Here that is a lock, not a promise. [`RegistryHandle::subscribe`] takes the shared state's lock,
//! performs **one** read of the registry, and from that single view it (a) flushes to existing
//! subscribers everything that changed since the last flush, (b) builds this subscriber's snapshot,
//! and (c) records the snapshot as the point this subscriber has been told about. A notification
//! cannot be produced between (b) and (c), because nothing else can hold the lock, so the new
//! subscriber can neither miss an event nor be told twice about one it already has.
//!
//! # What is deliberately not built
//!
//! `TreeSubscribeResult` has nowhere to say *"and there are N nodes I could not describe"*. Rather
//! than omit them into silence — the accept-and-ignore shape §11 item 23 keeps naming — the count is
//! kept on the supervisor's side and exposed as [`RegistryHandle::unprojectable`], where a test can
//! see it and a `doctor` will read it. Naming the gap is not the same as closing it, and this one is
//! open until the vocabulary has a field for it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use marion_core::agent_type;
use marion_core::contract::AgentId;
use marion_core::node::{NodeState, ReapState};
use marion_core::registry::{Replay, ReplayedNode};
use marion_proto::notify::Event;
use marion_proto::result::{NodeGetResult, TreeSubscribeResult};
use marion_proto::{
    Call, ClientGone, FailureKind, MethodResult, NodeSummary, ReplayPoint, RpcError,
};

use crate::registry::{LiveRegistry, Registry};
use crate::serve::{ConnId, Departure, Handle, Outbound};

/// Why a node the journal knows about cannot be described to a client.
///
/// An enum and not a `None`, because the three have different causes and different fixes, and a
/// client told only *"cannot describe it"* would have no idea whether to look at the journal, at
/// this build's agent types, or at a writer that produced a nonsense depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unprojectable {
    /// No `SpawnIntent`: the journal has records *about* this node and no identity *for* it. Its
    /// head was compacted, or the record was lost (§4.2's `Ordinal` loss is exactly this shape).
    NoIntent,
    /// The journal names an agent type this build does not have, so the node's `timeout` — §3.1's
    /// bound, which §9 re-resolves from the type — cannot be resolved from anywhere.
    UnknownAgentType(String),
    /// A depth `NodeSummary`'s `u8` cannot hold. §6.1's default `max_depth` is 3, so this is a
    /// writer producing nonsense rather than a deep tree, and saturating it would silently place the
    /// node somewhere it is not.
    DepthOutOfRange(u32),
}

impl Unprojectable {
    /// The refusal a client sees, with the citation for the fact that is missing.
    pub fn as_error(&self, agent: &AgentId) -> RpcError {
        match self {
            Unprojectable::NoIntent => RpcError::of(
                FailureKind::Internal,
                Some(&agent.0),
                "the journal has records about this node but no `SpawnIntent` for it, so marion \
                 does not know its agent type, its harness, its parent or its depth. Replay reports \
                 that absence rather than filling it in, and this call will not invent one \
                 (§4.2, §7.4).",
                "§4.2",
            ),
            Unprojectable::UnknownAgentType(t) => RpcError::not_found(
                t,
                format!(
                    "this node is recorded as agent type `{t}`, which this build of marion does \
                     not have. §3.1 makes the agent type the source of its timeout bound and §9 \
                     re-resolves that bound from the type rather than from a copy, so marion \
                     cannot describe the node without it."
                ),
                "§3.1",
            ),
            Unprojectable::DepthOutOfRange(d) => RpcError::of(
                FailureKind::Internal,
                Some(&agent.0),
                format!(
                    "this node is recorded at depth {d}, which is not a depth a tree marion built \
                     can reach (§6.1's default max_depth is 3). Reporting a saturated depth would \
                     place the node somewhere it is not."
                ),
                "§6.1",
            ),
        }
    }
}

/// §3.2's node, projected for a client — or the reason it cannot be.
///
/// Pure: it reads the replayed node and this build's agent-type registry, and touches nothing else.
/// That is what makes every arm above testable without a socket, a journal or a thread.
pub fn summarize(node: &ReplayedNode) -> Result<NodeSummary, Unprojectable> {
    let intent = node.intent.as_ref().ok_or(Unprojectable::NoIntent)?;
    let ty = agent_type::builtin(&intent.agent_type)
        .ok_or_else(|| Unprojectable::UnknownAgentType(intent.agent_type.clone()))?;
    let depth =
        u8::try_from(intent.depth).map_err(|_| Unprojectable::DepthOutOfRange(intent.depth))?;
    Ok(NodeSummary {
        agent_id: node.agent_id.clone(),
        parent_id: intent.parent_id.clone(),
        // Not a placeholder. See the module doc: nothing sets `Node.name` yet, so `None` is what
        // the journal says rather than what marion does not know.
        name: None,
        agent_type: intent.agent_type.clone(),
        harness: intent.harness,
        depth,
        state: node.state,
        reap_state: node.reap_state,
        timeout: ty.timeout,
    })
}

/// What a subscriber has already been told about one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Told {
    state: NodeState,
    reap_state: ReapState,
}

#[derive(Default)]
struct Shared {
    subs: Vec<Outbound>,
    told: HashMap<AgentId, Told>,
    unprojectable: usize,
}

/// A [`Handle`](crate::serve::Handle) backed by a running registry.
///
/// Everything a client can learn from it comes from the journal; nothing here probes a process,
/// consults a clock, or remembers a fact the journal does not carry.
pub struct RegistryHandle {
    live: Arc<LiveRegistry>,
    shared: Mutex<Shared>,
}

impl RegistryHandle {
    pub fn new(live: Arc<LiveRegistry>) -> Arc<RegistryHandle> {
        Arc::new(RegistryHandle {
            live,
            shared: Mutex::new(Shared::default()),
        })
    }

    /// How many nodes the journal knows about that marion could not describe to a client.
    ///
    /// See the module doc: `TreeSubscribeResult` has no field for this, so rather than dropping the
    /// nodes into silence the supervisor counts them here. It is the honest half of an admitted gap,
    /// not a substitute for closing it.
    pub fn unprojectable(&self) -> usize {
        lock(&self.shared).unprojectable
    }

    pub fn subscribers(&self) -> usize {
        lock(&self.shared).subs.len()
    }

    /// Push everything that changed since the last flush to every subscriber.
    ///
    /// Returns how many notifications were produced — **per event, not per delivery** — so a caller
    /// can tell "nothing changed" from "nothing was delivered", which are the same zero from the
    /// socket's side and different problems.
    pub fn flush(&self) -> usize {
        let mut g = lock(&self.shared);
        let events = self.live.read(|r| collect(r, &mut g));
        deliver(&mut g, &events);
        events.len()
    }

    /// §2's `tree/subscribe`: the snapshot, and the point live notifications begin from.
    fn subscribe(&self, out: &Outbound) -> TreeSubscribeResult {
        let mut g = lock(&self.shared);
        // **One read, three uses.** See the module doc: catching up existing subscribers, building
        // this one's snapshot, and recording what it has been told all happen against the same view,
        // under one lock, so there is no instant at which a notification could slip between the
        // snapshot and the subscription.
        let (events, nodes, read_point) = self.live.read(|r| {
            let events = collect(r, &mut g);
            let nodes = project(r.tree(), &mut g);
            (events, nodes, r.read_point())
        });
        deliver(&mut g, &events);
        g.subs.push(out.clone());
        TreeSubscribeResult { nodes, read_point }
    }

    fn node_get(&self, id: &AgentId) -> Result<NodeGetResult, RpcError> {
        self.live.read(|r| match r.tree().get(id) {
            None => Err(RpcError::not_found(
                &id.0,
                format!(
                    "this project's journal records no node `{}`. The registry is current as of \
                     {} records read; a node spawned by another process appears here once its \
                     `SpawnIntent` is on disk (§6.1 step 7).",
                    id.0,
                    r.read_point().records
                ),
                "§3.2",
            )),
            Some(node) => summarize(node)
                .map(|node| NodeGetResult { node })
                .map_err(|e| e.as_error(id)),
        })
    }
}

impl Handle for RegistryHandle {
    fn call(&self, _conn: ConnId, call: &Call, out: &Outbound) -> Result<MethodResult, RpcError> {
        match call {
            Call::NodeGet(p) => self.node_get(&p.agent_id).map(MethodResult::NodeGet),
            Call::TreeSubscribe(_) => Ok(MethodResult::TreeSubscribe(self.subscribe(out))),
            // Everything else is specified and not built. `Unimplemented` and not `Unsupported`,
            // per `error.rs`: the gap is marion's, not the harness's, and the operator's next move
            // is to check the milestone rather than the node.
            other => Err(RpcError::unimplemented(
                other.method().as_str(),
                format!(
                    "`{}` is specified (§2) and not built. This supervisor answers `node/get` and \
                     `tree/subscribe`; the rest of §2's fifteen land with the milestone that needs \
                     them.",
                    other.method().as_str()
                ),
                "§2",
            )),
        }
    }

    /// §7.3.1, and the whole of what this handler does about a departure.
    ///
    /// **Nothing happens to any node**, in either case. That is not an omission — for
    /// [`ClientGone::SocketClosed`] it is the invariant, stated as *"nodes do not change state and
    /// nothing is journaled, because from the registry's point of view nothing happened"*; and for
    /// [`ClientGone::Quit`] it is because §7.3.2's three dispositions are not built, which the
    /// `session/quit` call itself already said in words before the client closed.
    ///
    /// What does happen is bookkeeping this connection's subscription is dropped, so a supervisor
    /// with no clients holds no queues. §5.7 is explicit that this changes nothing about the
    /// supervisor's own lifetime.
    fn gone(&self, conn: ConnId, gone: &ClientGone, _why: &Departure) {
        debug_assert!(
            gone.nodes_must_be_untouched() || gone.disposition().is_some(),
            "§7.3.1 admits exactly two readings and both are handled"
        );
        let mut g = lock(&self.shared);
        g.subs.retain(|s| s.conn() != conn);
    }
}

/// The tree, as summaries, counting what could not be described.
fn project(tree: &Replay, g: &mut Shared) -> Vec<NodeSummary> {
    let mut out = Vec::new();
    let mut lost = 0usize;
    for n in tree.nodes() {
        match summarize(n) {
            Ok(s) => out.push(s),
            Err(_) => lost += 1,
        }
    }
    g.unprojectable = lost;
    out
}

/// What has changed since the last time anybody was told, in journal order.
///
/// Two events and not one: §2's `tree/node-added` exists because *"a client that learned of nodes
/// only from state changes would show a tree that is missing exactly the nodes currently being
/// created — the ones an operator is most likely watching."*
///
/// Every `ts` is the journal's, never this process's clock — see
/// [`marion_core::registry::ReplayedNode::first_ts`] for why a follower's `now()` is the wrong
/// answer on a field a client renders as when the thing occurred.
fn collect(r: &Registry, g: &mut Shared) -> Vec<Event> {
    let mut events = Vec::new();
    for n in r.tree().nodes() {
        let now = Told {
            state: n.state,
            reap_state: n.reap_state,
        };
        match g.told.get(&n.agent_id) {
            None => {
                // A node marion cannot describe produces no `tree/node-added` — there is no summary
                // to put in one — but it is still recorded as told, so it is not re-examined on
                // every flush. `project` is what counts it.
                if let Ok(node) = summarize(n) {
                    events.push(Event::NodeAdded {
                        node,
                        ts: journal_ts(n.first_ts),
                    });
                }
                g.told.insert(n.agent_id.clone(), now);
            }
            Some(before) if *before != now => {
                events.push(Event::NodeState {
                    agent_id: n.agent_id.clone(),
                    state: n.state,
                    reap_state: n.reap_state,
                    ts: journal_ts(n.state_ts),
                });
                g.told.insert(n.agent_id.clone(), now);
            }
            Some(_) => {}
        }
    }
    events
}

/// The journal's own time for a transition.
///
/// The `None` case is reachable and is not a decision this function may duck: a node replayed from
/// records written before `first_ts`/`state_ts` existed has neither. The epoch is used rather than
/// `now()` deliberately — a timestamp a client can *see* is wrong is better than one that is wrong
/// and plausible, which is the field-name-lies class this codebase refuses elsewhere.
fn journal_ts(ts: Option<marion_core::encoding::SystemTime>) -> marion_core::encoding::SystemTime {
    ts.unwrap_or_else(|| marion_core::encoding::SystemTime::from_unix_millis(0))
}

/// Send to every subscriber, dropping the ones that have gone.
///
/// [`Outbound::send`] never blocks, so this cannot be slowed by a client — see `serve.rs`: a full
/// queue is a verdict about that client, and §5.7 is what makes it the right one.
fn deliver(g: &mut Shared, events: &[Event]) {
    if events.is_empty() {
        return;
    }
    g.subs.retain(|s| {
        events.iter().all(|e| {
            s.send(&marion_proto::Frame::Notification(
                marion_proto::Notification::new(e.clone()),
            ))
        })
    });
}

/// A [`RegistryHandle`] flushed by a thread of its own.
///
/// The registry has its own follower (`LiveRegistry`) and this is a second loop over the result of
/// the first, which is deliberate: the follower's job is to be *current*, this one's is to be
/// *heard*, and a follower that also pushed would have a client's socket inside the lock that keeps
/// the tree current.
pub struct Broadcast {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Broadcast {
    /// Flush every `interval`.
    ///
    /// **The loop reads the stop flag before its flush and returns after it** — `LiveRegistry`'s
    /// rule, for the same reason: the transitions written just before a shutdown are exactly the
    /// ones a watching client cares about, and a loop that returned on the flag before flushing
    /// would drop them.
    pub fn start(handle: Arc<RegistryHandle>, interval: std::time::Duration) -> Broadcast {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                loop {
                    let done = stop.load(std::sync::atomic::Ordering::SeqCst);
                    handle.flush();
                    if done {
                        return;
                    }
                    std::thread::sleep(interval);
                }
            })
        };
        Broadcast {
            stop,
            thread: Some(thread),
        }
    }

    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Broadcast {
    fn drop(&mut self) {
        self.halt();
    }
}

/// A poisoned lock is taken, not unwrapped — `registry.rs`'s rule and §5.7's requirement.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The read point the registry is currently serving, for a caller that wants it without a client.
pub fn read_point(live: &LiveRegistry) -> ReplayPoint {
    live.read(|r| r.read_point())
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::{ExitStatus, ProcessExit};
    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::journal::{
        Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, StateChanged, WriterId, encode,
    };
    use marion_proto::Frame;
    use marion_testsupport::scratch;
    use std::io::Write;
    use std::path::Path;

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    fn line(seq: u64, ms: u64, kind: RecordKind) -> Vec<u8> {
        encode(&JournalRecord {
            writer: WriterId("w".into()),
            seq,
            ts: SystemTime::from_unix_millis(ms),
            mono_ns: seq,
            provenance: marion_core::ir::Provenance::marion(),
            src_seq: None,
            kind,
        })
        .expect("a record encodes")
    }

    /// The harness comes from the agent type, as a real writer's would — and the projection reads
    /// it back from the **journal**, not from the type, because the journal records what was
    /// actually launched.
    fn intent(agent: &str, parent: Option<&str>, ty: &str, depth: u32) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id(agent),
            parent_id: parent.map(id),
            agent_type: ty.into(),
            harness: agent_type::builtin(ty)
                .map(|t| t.harness)
                .unwrap_or(Harness::Codex),
            depth,
            task_id: None,
        })
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    fn replay_of(records: &[Vec<u8>]) -> Replay {
        let mut r = Replay::default();
        for b in records {
            r.extend(b);
        }
        r
    }

    fn node_of(records: &[Vec<u8>], agent: &str) -> ReplayedNode {
        replay_of(records).get(&id(agent)).unwrap().clone()
    }

    /// The projection, on a node the journal fully describes — including the two fields
    /// `registry.rs` refused to fabricate.
    #[test]
    fn a_summary_resolves_its_bound_from_the_agent_type_and_names_nothing_it_was_not_told() {
        let n = node_of(
            &[line(0, 1, intent("child", Some("root"), "codex-impl", 1))],
            "child",
        );
        let s = summarize(&n).expect("a fully described node projects");
        assert_eq!(s.agent_id, id("child"));
        assert_eq!(s.parent_id, Some(id("root")));
        assert_eq!(s.agent_type, "codex-impl");
        assert_eq!(s.harness, Harness::Codex);
        assert_eq!(s.depth, 1);
        assert_eq!(s.state, NodeState::Spawning);
        assert_eq!(
            s.timeout,
            agent_type::builtin("codex-impl").unwrap().timeout,
            "§3.1 makes the agent type the source of the bound and §9 re-resolves it from there; \
             a journalled copy would be a second source of truth"
        );
        assert_eq!(
            s.timeout,
            marion_core::encoding::Duration::from_secs(agent_type::DEFAULT_TIMEOUT_SECS),
            "and that source really is §3.1's 900 s default, not a value invented here"
        );
        assert_eq!(
            s.name, None,
            "nothing sets `Node.name` yet, so `None` is what the journal says rather than a \
             placeholder for what marion does not know"
        );
    }

    /// **NC — a node marion cannot describe is refused by name, never summarized with invented
    /// fields.**
    ///
    /// Three ways, three sentences. The failure this rules out is the tempting one: default the
    /// missing fields (`Harness::Codex`, depth 0, the 900 s bound) and hand back a summary that
    /// reads exactly like a real node's. A client cannot tell those apart, which is the
    /// partial-presented-as-complete shape this repo keeps refusing.
    #[test]
    fn an_undescribable_node_is_named_rather_than_filled_in() {
        // (1) records about a node, no identity for it.
        let orphan = node_of(
            &[line(
                0,
                1,
                RecordKind::Spawned(Spawned {
                    agent_id: id("no-intent"),
                    harness_version: "0.9.0".into(),
                    model: None,
                    pid: Some(3),
                }),
            )],
            "no-intent",
        );
        assert_eq!(summarize(&orphan), Err(Unprojectable::NoIntent));
        let e = Unprojectable::NoIntent.as_error(&id("no-intent"));
        assert_eq!(e.kind(), Some(FailureKind::Internal));
        assert!(e.message.contains("SpawnIntent"), "{e}");

        // (2) an agent type this build does not have — so §3.1's bound has no source.
        let unknown = node_of(&[line(0, 1, intent("a", None, "codex-turbo", 0))], "a");
        assert_eq!(
            summarize(&unknown),
            Err(Unprojectable::UnknownAgentType("codex-turbo".into()))
        );
        let e = Unprojectable::UnknownAgentType("codex-turbo".into()).as_error(&id("a"));
        assert_eq!(
            e.kind(),
            Some(FailureKind::NotFound),
            "`error.rs` names an agent type among the things NotFound is for"
        );
        assert!(e.message.contains("codex-turbo"), "{e}");

        // (3) a depth `NodeSummary`'s u8 cannot hold. §6.1's default max_depth is 3, so this is a
        // writer producing nonsense — and saturating it would place the node somewhere it is not.
        let deep = node_of(&[line(0, 1, intent("a", None, "codex-impl", 300))], "a");
        assert_eq!(summarize(&deep), Err(Unprojectable::DepthOutOfRange(300)));
        assert!(
            summarize(&node_of(
                &[line(0, 1, intent("a", None, "codex-impl", 255))],
                "a"
            ))
            .is_ok(),
            "255 fits, so the boundary is the type's and not an arbitrary cap"
        );
    }

    /// Build a handle over a journal file, plus the recording sink a subscriber would be.
    struct Fx {
        _dir: marion_testsupport::Scratch,
        path: std::path::PathBuf,
        handle: Arc<RegistryHandle>,
    }

    fn fx(tag: &str) -> Fx {
        let dir = scratch(tag);
        let path = dir.join("journal.jsonl");
        append(&path, &line(0, 1_000, intent("root", None, "claude", 0)));
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot_path(&path).unwrap(),
            std::time::Duration::from_millis(2),
        ));
        Fx {
            _dir: dir,
            path,
            handle: RegistryHandle::new(live),
        }
    }

    /// A pair of `Outbound`s is only obtainable from a live connection, so the subscription tests
    /// run a real server over a real socket — which is also the only way to assert that the
    /// notification reaches a *client* rather than a channel.
    struct Wired {
        fx: Fx,
        server: Option<crate::serve::Server>,
        dir: std::path::PathBuf,
        sock: std::path::PathBuf,
    }

    impl Wired {
        fn new(tag: &str) -> Wired {
            let fx = fx(tag);
            let dir = std::path::PathBuf::from(format!("/tmp/mh-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let paths = crate::socket::socket_paths(&dir, Path::new("/p"), 1);
            let crate::socket::Acquired::Serving(serving) = crate::socket::acquire(&paths).unwrap()
            else {
                panic!("nothing was listening")
            };
            let server = crate::serve::Server::start(
                serving,
                Arc::clone(&fx.handle) as Arc<dyn crate::serve::Handle>,
            );
            Wired {
                fx,
                server: Some(server),
                sock: paths.socket().to_path_buf(),
                dir,
            }
        }

        fn dial(&self) -> std::os::unix::net::UnixStream {
            let s = std::os::unix::net::UnixStream::connect(&self.sock).expect("dial");
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            s
        }
    }

    impl Drop for Wired {
        fn drop(&mut self) {
            if let Some(s) = self.server.take() {
                s.stop();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn call(s: &mut std::os::unix::net::UnixStream, call: Call, id: i64) {
        let f = Frame::Request(marion_proto::Request::new(
            marion_proto::RequestId::Number(id),
            call,
        ));
        s.write_all(f.to_line().as_bytes()).unwrap();
        s.flush().unwrap();
    }

    fn next_frame(r: &mut std::io::BufReader<std::os::unix::net::UnixStream>) -> Frame {
        use std::io::BufRead;
        let mut line = String::new();
        assert!(r.read_line(&mut line).unwrap() > 0, "the socket closed");
        Frame::from_line(&line).expect("well-formed")
    }

    fn until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        cond()
    }

    /// `node/get` over the socket, against a tree that came out of a journal — the request/response
    /// shape, end to end.
    #[test]
    fn node_get_answers_from_the_journal_and_refuses_a_node_it_has_no_record_of() {
        let w = Wired::new("handler-get");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());

        call(
            &mut c,
            Call::NodeGet(marion_proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::NodeGet(got) =
            marion_proto::Method::NodeGet.decode_result(&body).unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(got.node.agent_id, id("root"));
        assert_eq!(got.node.harness, Harness::ClaudeCode);
        assert_eq!(got.node.depth, 0);

        // A node the journal does not record is a refusal that says how much has been read, so an
        // operator can tell "no such node" from "not yet".
        call(
            &mut c,
            Call::NodeGet(marion_proto::params::NodeGetParams {
                agent_id: id("nobody"),
            }),
            2,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Error(e) = resp.outcome else {
            panic!("expected a refusal")
        };
        assert_eq!(e.kind(), Some(FailureKind::NotFound));
        assert!(e.is_refusal(), "a missing node is the caller's business");
        assert!(e.message.contains("records read"), "{e}");
    }

    /// **The subscription shape, against the real journal**: a snapshot, then a notification for a
    /// node that appeared afterwards — written by a *second* writer, which is the case the registry
    /// tails the file for in the first place.
    #[test]
    fn tree_subscribe_returns_a_snapshot_and_then_narrates_what_the_journal_says_next() {
        let w = Wired::new("handler-sub");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let _b = Broadcast::start(
            Arc::clone(&w.fx.handle),
            std::time::Duration::from_millis(2),
        );

        call(
            &mut c,
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(snap.nodes.len(), 1, "the root the journal already had");
        assert_eq!(snap.nodes[0].agent_id, id("root"));
        assert_eq!(snap.read_point.records, 1);

        // A different process appends a child and then moves it.
        append(
            &w.fx.path,
            &line(1, 2_000, intent("child", Some("root"), "codex-impl", 1)),
        );
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("a node that appeared must arrive as tree/node-added")
        };
        let Event::NodeAdded { node, ts } = n.event else {
            panic!("expected tree/node-added, got {:?}", n.event.method())
        };
        assert_eq!(node.agent_id, id("child"));
        assert_eq!(node.parent_id, Some(id("root")));
        assert_eq!(
            ts,
            SystemTime::from_unix_millis(2_000),
            "the journal's time, not the follower's — a client renders this as when it happened"
        );

        append(
            &w.fx.path,
            &line(
                2,
                3_500,
                RecordKind::StateChanged(StateChanged {
                    agent_id: id("child"),
                    state: NodeState::Running,
                }),
            ),
        );
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("expected a notification")
        };
        let Event::NodeState {
            agent_id,
            state,
            reap_state,
            ts,
        } = n.event
        else {
            panic!("expected node/state")
        };
        assert_eq!(agent_id, id("child"));
        assert_eq!(state, NodeState::Running);
        assert_eq!(
            reap_state,
            ReapState::Live,
            "§7.6 gates on the disjunction, so the two travel in one message"
        );
        assert_eq!(ts, SystemTime::from_unix_millis(3_500));
    }

    /// **NC — a node the snapshot could not describe is excluded *and counted*, never silently
    /// dropped.**
    ///
    /// `TreeSubscribeResult` has nowhere to say "and there are N I could not describe", so the count
    /// lives on the supervisor. The assertion is that it is not zero: a gap that is admitted is a
    /// different thing from a gap that is invisible, and the invisible version is the one §11 item
    /// 23 keeps naming.
    #[test]
    fn a_node_the_snapshot_cannot_describe_is_counted_rather_than_dropped_into_silence() {
        let w = Wired::new("handler-lost");
        // A node with records and no identity, written by a second writer.
        append(
            &w.fx.path,
            &line(
                1,
                2_000,
                RecordKind::Spawned(Spawned {
                    agent_id: id("headless"),
                    harness_version: "0.9.0".into(),
                    model: None,
                    pid: Some(9),
                }),
            ),
        );
        assert!(until(
            || w.fx.handle.live.read(|r| r.tree().nodes().len()) == 2
        ));

        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        call(
            &mut c,
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(
            snap.nodes
                .iter()
                .map(|n| n.agent_id.clone())
                .collect::<Vec<_>>(),
            [id("root")],
            "a node with no identity is not described"
        );
        assert_eq!(
            w.fx.handle.unprojectable(),
            1,
            "and it is not invisible either"
        );
    }

    /// A subscriber that goes away stops being one, so a supervisor with no clients holds no queues
    /// — and, per §7.3.1, nothing else happens at all.
    #[test]
    fn a_departed_client_stops_being_a_subscriber_and_nothing_else_changes() {
        let w = Wired::new("handler-gone");
        let before = w.fx.handle.live.read(|r| r.tree().nodes().len());
        {
            let mut c = w.dial();
            let mut r = std::io::BufReader::new(c.try_clone().unwrap());
            call(
                &mut c,
                Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
                1,
            );
            next_frame(&mut r);
            assert_eq!(w.fx.handle.subscribers(), 1);
        }
        assert!(
            until(|| w.fx.handle.subscribers() == 0),
            "a closed connection is not a subscriber"
        );
        assert_eq!(
            w.fx.handle.live.read(|r| r.tree().nodes().len()),
            before,
            "§7.3.1: from the registry's point of view nothing happened"
        );
    }

    /// A method that is specified and not built says so — `Unimplemented`, not `Unsupported`, and
    /// not silence.
    #[test]
    fn a_specified_but_unbuilt_method_is_refused_with_the_milestone_named() {
        let w = Wired::new("handler-unimpl");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        call(
            &mut c,
            Call::NodeCancel(marion_proto::params::NodeCancelParams {
                agent_id: id("root"),
            }),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Error(e) = resp.outcome else {
            panic!("expected a refusal")
        };
        assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
        assert!(e.message.contains("node/cancel"), "{e}");
    }

    /// **NC — a subscriber is told about a change exactly once, and a subscription that starts late
    /// is not told about what its own snapshot already contained.**
    ///
    /// §7.3.3's seam, as a test rather than as an argument: the snapshot and the point notifications
    /// begin from are taken under one lock from one read, so there is no instant between them for an
    /// event to be lost in or duplicated across.
    #[test]
    fn a_snapshot_and_its_subscription_meet_exactly_with_no_gap_and_no_overlap() {
        let w = Wired::new("handler-seam");
        // Two clients: one subscribes before the child appears, one after.
        let mut early = w.dial();
        let mut er = std::io::BufReader::new(early.try_clone().unwrap());
        call(
            &mut early,
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        next_frame(&mut er);

        append(
            &w.fx.path,
            &line(1, 2_000, intent("child", Some("root"), "codex-impl", 1)),
        );
        assert!(until(
            || w.fx.handle.live.read(|r| r.tree().nodes().len()) == 2
        ));

        let mut late = w.dial();
        let mut lr = std::io::BufReader::new(late.try_clone().unwrap());
        call(
            &mut late,
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );

        // The late subscriber's snapshot already has the child…
        let Frame::Response(resp) = next_frame(&mut lr) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(snap.nodes.len(), 2, "the snapshot is current, not stale");

        // …and the early subscriber was told about it, as the flush that the late subscribe
        // performed on its way in.
        let Frame::Notification(n) = next_frame(&mut er) else {
            panic!("the early subscriber must hear about the child")
        };
        assert_eq!(n.event.method(), "tree/node-added");

        // Now a change after both are subscribed reaches both, once each.
        append(
            &w.fx.path,
            &line(
                2,
                3_000,
                RecordKind::Exited(Exited {
                    agent_id: id("child"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "clean exit".into(),
                    },
                }),
            ),
        );
        assert!(until(|| w.fx.handle.flush() == 0
            && w.fx.handle.live.read(|r| r
                .tree()
                .get(&id("child"))
                .unwrap()
                .state
                .is_exited())));
        for r in [&mut er, &mut lr] {
            let Frame::Notification(n) = next_frame(r) else {
                panic!("both subscribers hear the exit")
            };
            let Event::NodeState { state, ts, .. } = n.event else {
                panic!("expected node/state")
            };
            assert_eq!(state, NodeState::Exited(ExitStatus::Ok));
            assert_eq!(ts, SystemTime::from_unix_millis(3_000));
        }
        // And exactly once: a second flush produces nothing, so nothing further arrives.
        assert_eq!(w.fx.handle.flush(), 0, "a told transition is not re-told");
    }
}
