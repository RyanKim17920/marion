//! **§2's `node/steer`** — a message for a node's next turn, from the operator or from a node above
//! it, queued in the node's inbox (`crate::inbox`).
//!
//! # Who may steer whom (§5.4)
//!
//! * **`caller: None` is a top-level peer** — the TUI, the CLI, a `marion mcp` an operator
//!   configured. It is authorized exactly as a root spawn is ([`super::root_spawn_authorized`]:
//!   the socket's filesystem permission, checked against peer credentials), and its scope is the
//!   whole project, the same rule `mcp.rs`'s `list_scope` gives a top-level client.
//! * **`caller: Some` is a node**, proved by the token marion minted for it ([`RegistryHandle::
//!   authenticate`], the check `agent/spawn` uses, token first and decoy-timed), and it may steer
//!   only a **strict descendant**: the target's `parent_id` chain must reach the caller. Itself, a
//!   sibling, an ancestor, an unknown target, an unknown caller and a wrong token are all refused
//!   with one error, so the refusal says nothing about which it was.
//!
//! # What "accepted" means in this build
//!
//! The message is journaled (`MessageQueued`, length and digest only) and waits for the node's
//! next turn boundary. A lane that delivers attaches a [`crate::inbox::DeliveryPort`]; a node whose
//! lane has none yet keeps the message until it ends, and it is then dropped, by name, on the
//! journal. The result says `queued: true` and never claims a delivery — `MessageDelivered` does,
//! later. **Wired:** a `TerminalPaste` node whose terminal this supervisor hosts (a pane, or a
//! native `marion <harness>` session) gets a [`crate::paste::PasteInjector`] the moment its pane
//! is published ([`RegistryHandle::attach_paste_delivery`]).

use marion_core::contract::{AgentId, TaskContract, TaskId};
use marion_core::harness::Harness;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::params::NodeSteerParams;
use marion_core::proto::result::DeliveryResult;
use marion_core::proto::{Delivery, RpcError, SpawnCaller};
use marion_core::registry::Replay;
use marion_harness::spec::{NodeShape, TurnDelivery, delivery_for};
use std::sync::Arc;

use super::{RegistryHandle, lock, root_spawn_authorized};
use crate::inbox::{Refusal, Source};
use crate::serve::Peer;

impl RegistryHandle {
    pub(super) fn node_steer(
        &self,
        p: &NodeSteerParams,
        peer: Peer,
    ) -> Result<DeliveryResult, RpcError> {
        self.live.refresh();
        let source = match &p.caller {
            None => {
                root_spawn_authorized(peer)?;
                Source::Operator
            }
            Some(c) => self.steering_ancestor(c, &p.agent_id)?,
        };
        let node = self
            .live
            .read(|r| r.tree().get(&p.agent_id).cloned())
            .ok_or_else(|| {
                RpcError::refused(
                    "agent_id",
                    format!(
                        "no node `{}` is on this project's journal, so there is nothing to steer.",
                        p.agent_id.0
                    ),
                    "§2",
                )
            })?;
        if node.state.is_exited() || node.reap_state != ReapState::Live {
            return Err(refusal(Refusal::Ended));
        }
        if node.state == NodeState::Spawning {
            return Err(refusal(Refusal::NotReady));
        }
        let Some(harness) = node.harness() else {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` has records on this journal but no `SpawnIntent`, so marion cannot say \
                     which harness would take the message.",
                    p.agent_id.0
                ),
                "§4.3",
            ));
        };
        let delivery = self.delivery_of(&p.agent_id, harness);
        // A node this supervisor's `agent/spawn` did not launch — a native `marion <harness>`
        // session, or one an earlier supervisor launched — has no inbox here. The gap is marion's,
        // so it is `Unimplemented`, never "retry": waiting would not open one.
        if !matches!(delivery, TurnDelivery::None { .. })
            && self.owned_running(&p.agent_id).is_none()
        {
            return Err(RpcError::unimplemented(
                "node/steer",
                format!(
                    "marion holds no inbox for `{}`: this supervisor's `agent/spawn` did not launch \
                     it (a native session, or a node an earlier supervisor launched), and turn \
                     delivery to such a node is not built yet.",
                    p.agent_id.0
                ),
                "§6.3",
            ));
        }
        let message_id = self
            .inboxes
            .enqueue(&p.agent_id, delivery, source, p.text.clone())
            .map_err(refusal)?;
        Ok(DeliveryResult {
            delivered_as: Delivery::Steer,
            state: node.state,
            resumed: false,
            message_id: Some(message_id),
            queued: true,
        })
    }

    /// How a message reaches `agent`'s next turn: its harness's row, in the shape marion is
    /// running it in — interactive iff marion hosts a live terminal for it (a pane, or a native
    /// session's).
    fn delivery_of(&self, agent: &AgentId, harness: Harness) -> TurnDelivery {
        let shape = if lock(&self.panes).has_live(agent) {
            NodeShape::Interactive
        } else {
            NodeShape::Headless
        };
        delivery_for(marion_harness::adapter::harness_spec(harness), shape)
    }

    /// **`agent`'s terminal was just published: start its paste driver if its row takes turns by
    /// pasting.** The shape is interactive by construction — `host` is the terminal — so the row's
    /// interactive strategy decides, and only a `TerminalPaste` row gets a
    /// [`crate::paste::PasteInjector`]. A node with no open inbox (one this supervisor did not
    /// claim) gets none: the injector releases its thread when `attach_port` refuses.
    pub(super) fn attach_paste_delivery(&self, agent: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.live.refresh();
        let Some((harness, bound)) = self.live.read(|r| {
            let n = r.tree().get(agent)?;
            Some((n.harness()?, node_bound(n)))
        }) else {
            return;
        };
        let row = marion_harness::adapter::harness_spec(harness);
        if let Some(params) = crate::paste::PasteParams::for_row(row) {
            // A boot dialog marion may not answer waits for the operator as long as the node may
            // run; every other boot wait keeps the paste grace.
            let policy = match bound {
                Some(bound) => crate::paste::PastePolicy::for_node(bound),
                None => crate::paste::PastePolicy::PRODUCTION,
            };
            let _ = crate::paste::PasteInjector::start(
                agent.clone(),
                host,
                &self.inboxes,
                params,
                policy,
            );
        }
    }

    /// **A backgrounded child ended: queue its end for its parent's next turn** — the text the
    /// parent's `wait` on that task returns, from [`Source::ChildEnded`] — and settle the debt the
    /// parent's inbox was held open by ([`crate::inbox::Inboxes::announce`]).
    ///
    /// Exactly one path announces it ([`announcement_route`]): a parent on Claude Code's channel
    /// gets the bridge's push and nothing here, and a parent whose row has no strategy gets
    /// nothing. A parent that already ended is refused by its sealed inbox, which journals nothing
    /// — there is no queued message to resolve.
    pub(crate) fn announce_child_end(&self, parent: &AgentId, end: ChildEnd<'_>) {
        self.live.refresh();
        let harness = self
            .live
            .read(|r| r.tree().get(parent).and_then(|n| n.harness()));
        let delivery = harness.map(|h| self.delivery_of(parent, h));
        match delivery.map(announcement_route) {
            Some(AnnouncementRoute::Inbox) => {}
            _ => {
                self.inboxes.release(parent);
                return;
            }
        }
        let (body, _) = crate::bridge::spawn_text_of(end.agent_type, end.outcome);
        let status = match end.outcome {
            Ok(c) => c.completion.as_ref().map_or_else(
                || "failed".to_string(),
                |comp| crate::mcp::status_word(comp.status),
            ),
            Err(_) => "failed".to_string(),
        };
        let source = Source::ChildEnded {
            child: end.child.clone(),
            task_id: end.task_id.clone(),
            status,
            agent_type: end.agent_type.to_string(),
            root: false,
        };
        let delivery = delivery.expect("the inbox route has a delivery");
        if let Err(r) = self.inboxes.announce(parent, delivery, source, body) {
            // Not an error of the child's: the parent ended first, or the journal refused.
            eprintln!(
                "marion: `{}`'s end was not queued for its parent `{}`: {}",
                end.child.0,
                parent.0,
                r.sentence()
            );
        }
    }

    /// The caller, if it proved to be a node this supervisor minted a token for **and** a strict
    /// ancestor of `target`; the one §5.4 refusal otherwise.
    fn steering_ancestor(&self, c: &SpawnCaller, target: &AgentId) -> Result<Source, RpcError> {
        let authentic = self.authenticate(c).is_some();
        let agent_type = self.live.read(|r| {
            (authentic && is_strict_ancestor(r.tree(), &c.agent_id, target))
                .then(|| r.tree().get(&c.agent_id))
                .flatten()
                .and_then(|n| n.intent.as_ref().map(|i| i.agent_type.clone()))
        });
        match agent_type {
            Some(agent_type) => Ok(Source::Ancestor {
                agent_id: c.agent_id.clone(),
                agent_type,
            }),
            None => Err(RpcError::refused(
                "caller",
                "a node may steer only a node below it in the tree (§5.4), and marion could not \
                 establish that of this caller: either its token is not one this supervisor minted \
                 for the node it names, or the target is not its descendant. One refusal for every \
                 case, so it says nothing about which.",
                "§5.4",
            )),
        }
    }
}

/// One backgrounded child's end, as [`RegistryHandle::announce_child_end`] words it.
pub(crate) struct ChildEnd<'a> {
    pub child: &'a AgentId,
    pub agent_type: &'a str,
    pub task_id: &'a TaskId,
    pub outcome: &'a Result<TaskContract, crate::spawn::SpawnError>,
}

/// Which one path a child's end takes to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnouncementRoute {
    /// Queued in the parent's inbox, for the lane that drives it.
    Inbox,
    /// The parent's bridge already pushes it (Claude Code's channel), so queuing it too would
    /// deliver it twice.
    Pushed,
    /// The parent's row has no measured way to take it.
    Nowhere,
}

/// The route for a parent taking turns by `delivery`. No harness is named: the row decides.
pub(crate) fn announcement_route(delivery: TurnDelivery) -> AnnouncementRoute {
    match delivery {
        TurnDelivery::TypedTurn { .. }
        | TurnDelivery::Continuation { .. }
        | TurnDelivery::TerminalPaste { .. } => AnnouncementRoute::Inbox,
        TurnDelivery::McpChannel { .. } => AnnouncementRoute::Pushed,
        TurnDelivery::None { .. } => AnnouncementRoute::Nowhere,
    }
}

/// Whether `ancestor` appears on `target`'s `parent_id` chain — never `target` itself. An unknown
/// target has no chain. `visited` makes termination a property of the loop rather than of the
/// journal (§7.5 fixes a parent at its `SpawnIntent`, so a cycle cannot arise from marion's own).
fn is_strict_ancestor(tree: &Replay, ancestor: &AgentId, target: &AgentId) -> bool {
    let mut visited = vec![target.clone()];
    let mut cur = tree.get(target).and_then(|n| n.parent_id().cloned());
    while let Some(id) = cur {
        if &id == ancestor {
            return true;
        }
        if visited.contains(&id) {
            return false;
        }
        visited.push(id.clone());
        cur = tree.get(&id).and_then(|n| n.parent_id().cloned());
    }
    false
}

/// An inbox refusal, as the wire says it.
fn refusal(r: Refusal) -> RpcError {
    match &r {
        Refusal::Ended => RpcError::refused("agent_id", r.sentence(), "§6.3, §8"),
        Refusal::Unsupported { .. } => RpcError::unsupported("agent_id", r.sentence(), "§6.3"),
        Refusal::NotReady => RpcError::refused("agent_id", r.sentence(), "§6.3"),
        Refusal::Journal(_) => RpcError::internal(r.sentence()),
    }
}

/// The node's wall-clock bound: the one its launch recorded, else its built-in type's default —
/// the reading the tree's summary makes (`handler::summarize`). `None` for a node the journal
/// gives neither.
fn node_bound(n: &marion_core::registry::ReplayedNode) -> Option<std::time::Duration> {
    let intent = n.intent.as_ref()?;
    match intent.timeout_secs {
        Some(secs) => Some(std::time::Duration::from_secs(secs)),
        None => marion_core::agent_type::builtin(&intent.agent_type).map(|t| t.timeout.0),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::journal::{
        Exited, JournalRecord, MessageSource, RecordKind, SpawnIntent, Spawned, StateChanged,
        WriterId, encode,
    };
    use marion_core::proto::{Call, FailureKind, MethodResult};
    use marion_testsupport::{append, scratch};

    use super::*;
    use crate::registry::{LiveRegistry, Registry};
    use crate::serve::{ConnId, Handle};

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    struct Fx {
        _dir: marion_testsupport::Scratch,
        path: std::path::PathBuf,
        handle: Arc<RegistryHandle>,
        seq: std::cell::Cell<u64>,
    }

    impl Fx {
        fn write(&self, kind: RecordKind) {
            let seq = self.seq.get();
            self.seq.set(seq + 1);
            let line = encode(&JournalRecord {
                writer: WriterId("w".into()),
                seq,
                ts: SystemTime::from_unix_millis(1_000 + seq),
                mono_ns: seq,
                provenance: marion_core::ir::Provenance::marion(),
                src_seq: None,
                kind,
            })
            .unwrap();
            append(&self.path, &line);
        }

        /// A node journaled as running under `harness`, and claimed — so it has a token and an
        /// inbox, as every node this supervisor launches does.
        fn running(&self, agent: &str, parent: Option<&str>, harness: Harness) -> String {
            self.intent(agent, parent, harness);
            self.write(RecordKind::Spawned(Spawned {
                agent_id: id(agent),
                harness_version: "test".into(),
                model: None,
                pid: Some(4242),
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }));
            self.write(RecordKind::StateChanged(StateChanged {
                agent_id: id(agent),
                state: NodeState::Running,
                reason: None,
            }));
            self.handle.live.refresh();
            self.handle
                .claim(&id(agent), None, "/repo".into())
                .expose()
                .to_string()
        }

        fn intent(&self, agent: &str, parent: Option<&str>, harness: Harness) {
            self.write(RecordKind::SpawnIntent(SpawnIntent {
                review_of: None,
                agent_id: id(agent),
                parent_id: parent.map(id),
                agent_type: format!("type-{agent}"),
                harness,
                depth: u32::from(parent.is_some()),
                task_id: None,
                timeout_secs: None,
                verification: vec![],
            }));
            self.handle.live.refresh();
        }

        fn steer(
            &self,
            target: &str,
            caller: Option<(&str, &str)>,
        ) -> Result<DeliveryResult, RpcError> {
            let out = crate::serve::sink(ConnId(7));
            let call = Call::NodeSteer(NodeSteerParams {
                agent_id: id(target),
                text: "use the v2 API".into(),
                caller: caller.map(|(agent, token)| SpawnCaller {
                    agent_id: id(agent),
                    node_token: token.into(),
                }),
            });
            match self.handle.call(ConnId(7), &call, &out)? {
                MethodResult::NodeSteer(r) => Ok(r),
                other => panic!("wrong result: {}", other.method().as_str()),
            }
        }

        fn queued_records(&self) -> Vec<marion_core::journal::MessageQueued> {
            std::fs::read(&self.path)
                .unwrap()
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .filter_map(marion_core::journal::decode)
                .filter_map(|r| match r.kind {
                    RecordKind::MessageQueued(q) => Some(q),
                    _ => None,
                })
                .collect()
        }
    }

    /// The registry boots before the records are written — the production order (a node already
    /// live at boot would be marked `Orphaned` by §7.2's restart pass).
    fn fx(tag: &str) -> Fx {
        let dir = scratch(tag);
        let path = dir.join("journal.jsonl");
        let live = Arc::new(LiveRegistry::follow(Registry::boot_path(&path).unwrap()));
        Fx {
            _dir: dir,
            path,
            handle: RegistryHandle::new(live),
            seq: std::cell::Cell::new(0),
        }
    }

    /// root → child → grandchild, and root → sibling; all claude, all running.
    fn family(tag: &str) -> (Fx, [String; 4]) {
        let fx = fx(tag);
        let root = fx.running("root", None, Harness::ClaudeCode);
        let child = fx.running("child", Some("root"), Harness::ClaudeCode);
        let grand = fx.running("grand", Some("child"), Harness::ClaudeCode);
        let sibling = fx.running("sibling", Some("root"), Harness::ClaudeCode);
        (fx, [root, child, grand, sibling])
    }

    fn assert_queued(r: Result<DeliveryResult, RpcError>) -> String {
        let r = r.expect("accepted");
        assert!(
            r.queued,
            "accepted for the next turn, never claimed delivered"
        );
        assert_eq!(r.delivered_as, Delivery::Steer);
        assert!(!r.resumed);
        r.message_id.expect("the id its journal records carry")
    }

    /// **§5.4's matrix: the operator, a parent and a grandparent are accepted; everything else is
    /// refused with one indistinguishable error.**
    #[test]
    fn only_the_operator_and_strict_ancestors_may_steer_a_node() {
        let (fx, [root, child, grand, sibling]) = family("steer-auth");

        let by_operator = assert_queued(fx.steer("child", None));
        let by_parent = assert_queued(fx.steer("child", Some(("root", &root))));
        let by_grandparent = assert_queued(fx.steer("grand", Some(("root", &root))));
        assert_ne!(by_operator, by_parent);

        let queued = fx.queued_records();
        let sources: Vec<_> = queued
            .iter()
            .map(|q| (q.message_id.clone(), q.agent_id.0.clone(), q.source.clone()))
            .collect();
        assert_eq!(
            sources,
            vec![
                (by_operator, "child".into(), MessageSource::Operator),
                (
                    by_parent,
                    "child".into(),
                    MessageSource::Ancestor(id("root"))
                ),
                (
                    by_grandparent,
                    "grand".into(),
                    MessageSource::Ancestor(id("root"))
                ),
            ]
        );

        let forbidden = [
            ("self", fx.steer("child", Some(("child", &child)))),
            ("sibling", fx.steer("child", Some(("sibling", &sibling)))),
            ("child to parent", fx.steer("root", Some(("child", &child)))),
            (
                "grandchild to grandparent",
                fx.steer("root", Some(("grand", &grand))),
            ),
            (
                "wrong token",
                fx.steer("child", Some(("root", "0".repeat(64).as_str()))),
            ),
            ("unknown caller", fx.steer("child", Some(("nobody", &root)))),
            ("unknown target", fx.steer("nobody", Some(("root", &root)))),
        ];
        let first = forbidden[0].1.clone().expect_err("refused");
        assert_eq!(first.kind(), Some(FailureKind::Refused));
        assert!(first.message.contains("§5.4"), "{first}");
        for (case, r) in forbidden {
            assert_eq!(
                r.expect_err(case),
                first,
                "{case}: the same error as every other forbidden steer"
            );
        }
        assert_eq!(fx.queued_records().len(), 3, "a refusal queues nothing");
    }

    /// The journal records the length and digest of what was steered, never the words.
    #[test]
    fn a_steer_is_journaled_by_length_and_digest_only() {
        let (fx, _) = family("steer-digest");
        assert_queued(fx.steer("child", None));
        let q = &fx.queued_records()[0];
        assert_eq!(q.len as usize, "use the v2 API".len());
        assert_eq!(q.sha256, crate::inbox::sha256_hex(b"use the v2 API"));
        let journal = std::fs::read_to_string(&fx.path).unwrap();
        assert!(!journal.contains("v2 API"), "{journal}");
    }

    /// An ended node refuses and points at `node/resume`; a spawning one says retry; a harness
    /// with no measured strategy is `Unsupported`, quoting its row.
    #[test]
    fn a_steer_the_node_cannot_take_is_refused_by_name() {
        let fx = fx("steer-states");
        fx.running("done", None, Harness::ClaudeCode);
        fx.write(RecordKind::Exited(Exited {
            agent_id: id("done"),
            status: marion_core::contract::ExitStatus::Ok,
            exit: marion_core::contract::ProcessExit {
                code: Some(0),
                signal: None,
                description: "clean exit".into(),
            },
        }));
        fx.handle.live.refresh();
        let ended = fx.steer("done", None).expect_err("ended");
        assert!(ended.message.contains("node/resume"), "{ended}");

        fx.intent("spawning", None, Harness::ClaudeCode);
        let spawning = fx.steer("spawning", None).expect_err("spawning");
        assert!(spawning.message.contains("retry"), "{spawning}");

        fx.running("gem", None, Harness::Gemini);
        fx.intent("native", None, Harness::ClaudeCode);
        for kind in [
            RecordKind::Spawned(Spawned {
                agent_id: id("native"),
                harness_version: "test".into(),
                model: None,
                pid: Some(4243),
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }),
            RecordKind::StateChanged(StateChanged {
                agent_id: id("native"),
                state: NodeState::Running,
                reason: None,
            }),
        ] {
            fx.write(kind);
        }
        fx.handle.live.refresh();
        let unowned = fx.steer("native", None).expect_err("no inbox");
        assert_eq!(unowned.kind(), Some(FailureKind::Unimplemented));
        assert!(!unowned.message.contains("retry"), "{unowned}");

        let unsupported = fx.steer("gem", None).expect_err("no strategy");
        assert_eq!(unsupported.kind(), Some(FailureKind::Unsupported));
        let note = marion_harness::adapter::harness_spec(Harness::Gemini)
            .delivery
            .headless
            .note();
        assert!(unsupported.message.contains(note), "{unsupported}");
        assert!(fx.queued_records().is_empty());
    }

    /// A message still queued when its node ends is dropped, by name, on the journal.
    #[test]
    fn a_message_waiting_when_its_node_ends_is_dropped_on_the_journal() {
        let (fx, _) = family("steer-close");
        let m = assert_queued(fx.steer("child", None));
        fx.handle
            .mark_finished(&id("child"), super::super::NodeOutcome::Root(Ok(())));
        let dropped: Vec<_> = std::fs::read(&fx.path)
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .filter_map(marion_core::journal::decode)
            .filter_map(|r| match r.kind {
                RecordKind::MessageDropped(d) => Some(d.message_id),
                _ => None,
            })
            .collect();
        assert_eq!(dropped, vec![m]);
    }

    /// **A backgrounded child's end, queued for its parent's next turn** — worded exactly as the
    /// parent's `wait` on that task would answer, with the journal naming who it is from.
    #[test]
    fn a_background_childs_end_is_queued_for_its_parent_as_its_wait_text() {
        let (fx, _) = family("announce-queued");
        assert!(fx.handle.inboxes.owe(&id("root")));
        let outcome = Err(crate::spawn::SpawnError::NoContract {
            path: "/nowhere".into(),
            why: "the test's child left none".into(),
        });
        let task = marion_core::contract::TaskId("t-1".into());
        fx.handle.announce_child_end(
            &id("root"),
            super::ChildEnd {
                child: &id("child"),
                agent_type: "type-child",
                task_id: &task,
                outcome: &outcome,
            },
        );
        let m = fx.handle.inboxes.take_next(&id("root")).expect("queued");
        let (wait_text, _) = crate::bridge::spawn_text_of("type-child", &outcome);
        assert_eq!(
            crate::inbox::render(&m),
            crate::inbox::child_ended_text("type-child", false, "t-1", &wait_text)
        );
        assert_eq!(
            fx.queued_records()[0].source,
            MessageSource::ChildEnded {
                child: id("child"),
                task_id: task,
                status: "failed".into(),
            }
        );
        assert!(
            fx.handle.inboxes.held(&id("root")),
            "the debt is settled, the message still waits: open"
        );
    }

    /// **Exactly one path announces a child's end.** A parent whose lane is Claude Code's channel
    /// already gets the bridge's push, so the inbox must not queue a second copy; a parent with no
    /// strategy gets nothing queued; every lane marion drives from the inbox gets it there.
    #[test]
    fn a_childs_end_takes_exactly_one_route_to_its_parent() {
        use super::AnnouncementRoute::{Inbox, Nowhere, Pushed};
        use marion_harness::spec::{MidTurn, TurnDelivery};
        let n = "n";
        for (d, want) in [
            (
                TurnDelivery::TypedTurn {
                    mid_turn: MidTurn::Fold,
                    note: n,
                },
                Inbox,
            ),
            (TurnDelivery::Continuation { note: n }, Inbox),
            (
                TurnDelivery::bracketed_paste(marion_harness::spec::BootSignal::FirstDraw, n),
                Inbox,
            ),
            (TurnDelivery::McpChannel { note: n }, Pushed),
            (TurnDelivery::None { note: n }, Nowhere),
        ] {
            assert_eq!(super::announcement_route(d), want, "{d:?}");
        }
    }

    /// A parent that already ended is told nothing and journals nothing: its inbox is sealed, and
    /// the announcement settles no debt it could still be holding open for.
    #[test]
    fn a_childs_end_for_a_parent_that_ended_is_not_queued() {
        let (fx, _) = family("announce-ended");
        fx.handle
            .mark_finished(&id("root"), super::super::NodeOutcome::Root(Ok(())));
        let outcome = Err(crate::spawn::SpawnError::NodeAborted("gone".into()));
        fx.handle.announce_child_end(
            &id("root"),
            super::ChildEnd {
                child: &id("child"),
                agent_type: "type-child",
                task_id: &marion_core::contract::TaskId("t-2".into()),
                outcome: &outcome,
            },
        );
        assert!(fx.queued_records().is_empty());
    }

    /// **A node's driver reaches exactly its own inbox through its owner** — what `node/steer`
    /// queues for the node is what the driver takes, and another node's queue is not visible.
    #[test]
    fn a_nodes_owner_hands_its_driver_the_nodes_own_inbox() {
        use crate::run::SpawnObserver;
        let (fx, _) = family("owner-turns");
        let (tx, _rx) = std::sync::mpsc::channel();
        let owner = super::super::NodeOwner {
            handle: Arc::clone(&fx.handle),
            task_id: None,
            repo: "/repo".into(),
            tx,
            identified: std::sync::Mutex::new(None),
            announce_to: None,
            owes: Default::default(),
        };
        let turns = owner
            .turn_source(&id("child"))
            .expect("the supervisor keeps an inbox");
        let m = assert_queued(fx.steer("child", None));
        assert_queued(fx.steer("grand", None));
        assert_eq!(turns.take_next().map(|m| m.id), Some(m));
        assert_eq!(
            turns.take_next(),
            None,
            "the grandchild's message is not the child's"
        );
    }

    /// A raw pty whose host is `agent`'s terminal, with the node side already asking for
    /// bracketed paste, a prompt drawn and its window title set — what a TUI does at boot (the
    /// title is codex's boot mark). Not registered: the caller does that.
    fn pasting_terminal(
        fx: &Fx,
        agent: &str,
        tag: &str,
    ) -> (Arc<crate::pty::PtyHost>, std::fs::File) {
        use std::io::Write;
        let size = crate::pty::WinSize::new(80, 24);
        let master = crate::pty::PtyMaster::open(size).unwrap();
        let mut slave = std::fs::File::from(master.open_slave().unwrap());
        let mut termios = rustix::termios::tcgetattr(&slave).unwrap();
        termios.make_raw();
        rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &termios)
            .unwrap();
        let host = Arc::new(
            crate::pty::PtyHost::start(
                id(agent),
                master,
                &fx._dir.join(format!("{tag}.cast")),
                size,
                "xterm-256color",
                std::time::Instant::now(),
            )
            .unwrap(),
        );
        slave.write_all(b"\x1b[?2004h> \x1b]0;pane\x07").unwrap();
        assert!(marion_testsupport::until(|| host.bracketed_paste()));
        (host, slave)
    }

    /// Every `via` the journal gives message `m`'s deliveries.
    fn deliveries(fx: &Fx, m: &str) -> Vec<String> {
        std::fs::read(&fx.path)
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .filter_map(marion_core::journal::decode)
            .filter_map(|r| match r.kind {
                RecordKind::MessageDelivered(d) if d.message_id == m => Some(d.via),
                _ => None,
            })
            .collect()
    }

    fn operator_paste(text: &str) -> Vec<u8> {
        let mut want =
            crate::paste::frame(&format!("marion: message from the operator: {text}")).into_bytes();
        want.push(b'\r');
        want
    }

    /// **A steer into a codex pane is typed into its terminal.** The pane's registration is what
    /// attaches the row's `TerminalPaste` driver: the operator's message reaches the node's pty as
    /// one bracketed paste of the rendered text and a CR, and is journaled delivered by paste.
    #[test]
    fn a_steer_into_a_terminal_paste_pane_is_pasted_into_its_pty() {
        let fx = fx("steer-paste");
        fx.running("pane", None, Harness::Codex);
        let (host, mut slave) = pasting_terminal(&fx, "pane", "pane");
        fx.handle.register_pane(&id("pane"), Arc::clone(&host));

        let m = assert_queued(fx.steer("pane", None));
        let want = operator_paste("use the v2 API");
        let got = crate::paste::tests::read_slave_within(
            &mut slave,
            want.len(),
            std::time::Duration::from_secs(20),
        );
        assert_eq!(got, want);
        assert!(marion_testsupport::until(|| !deliveries(&fx, &m).is_empty()));
        assert_eq!(deliveries(&fx, &m), vec![crate::paste::VIA.to_string()]);
        fx.handle
            .mark_finished(&id("pane"), super::super::NodeOutcome::Root(Ok(())));
    }

    /// **A replaced pane takes the paste, not the terminal it replaced** — a resume re-registers
    /// the node's pane, and the new host's injector becomes the inbox's port (the old one is
    /// closed by the replacement), so the message is typed once, into the live terminal.
    #[test]
    fn a_replaced_pane_gets_the_paste_and_the_old_one_does_not() {
        let fx = fx("steer-paste-replaced");
        fx.running("pane", None, Harness::Codex);
        let (old, _old_slave) = pasting_terminal(&fx, "pane", "old");
        fx.handle.register_pane(&id("pane"), Arc::clone(&old));
        let (new, mut slave) = pasting_terminal(&fx, "pane", "new");
        fx.handle.register_pane(&id("pane"), Arc::clone(&new));

        let m = assert_queued(fx.steer("pane", None));
        let want = operator_paste("use the v2 API");
        let got = crate::paste::tests::read_slave_within(
            &mut slave,
            want.len(),
            std::time::Duration::from_secs(20),
        );
        assert_eq!(got, want);
        assert!(marion_testsupport::until(|| !deliveries(&fx, &m).is_empty()));
        assert_eq!(deliveries(&fx, &m).len(), 1, "typed once");
        fx.handle
            .mark_finished(&id("pane"), super::super::NodeOutcome::Root(Ok(())));
    }

    /// **A pane whose row does not paste gets no injector**: claude's interactive row is its MCP
    /// channel, so a steer into a claude pane is queued and nothing is typed into its terminal.
    #[test]
    fn a_pane_whose_row_does_not_paste_is_never_typed_into() {
        let fx = fx("steer-no-paste");
        fx.running("pane", None, Harness::ClaudeCode);
        let (host, _slave) = pasting_terminal(&fx, "pane", "pane");
        fx.handle.register_pane(&id("pane"), host);
        let m = assert_queued(fx.steer("pane", None));
        fx.handle
            .mark_finished(&id("pane"), super::super::NodeOutcome::Root(Ok(())));
        assert!(deliveries(&fx, &m).is_empty());
        let dropped = std::fs::read(&fx.path)
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .filter_map(marion_core::journal::decode)
            .any(|r| matches!(r.kind, RecordKind::MessageDropped(d) if d.message_id == m));
        assert!(
            dropped,
            "the message waited for a lane and went down with the node"
        );
    }

    /// `node/prompt` stays unbuilt, and its refusal points at `node/steer`.
    #[test]
    fn node_prompt_is_unimplemented_and_points_at_steer() {
        let fx = fx("steer-prompt");
        let out = crate::serve::sink(ConnId(7));
        let call = Call::NodePrompt(marion_core::proto::params::NodePromptParams {
            agent_id: id("x"),
            text: "hi".into(),
        });
        let e = fx.handle.call(ConnId(7), &call, &out).expect_err("unbuilt");
        assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
        assert!(e.message.contains("node/steer"), "{e}");
    }
}
