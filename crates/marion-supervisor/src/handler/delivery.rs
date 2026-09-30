use std::path::PathBuf;
use std::sync::Arc;

use marion_core::contract::{AgentId, TaskId};
use marion_core::proto::notify::Event;
use marion_core::secret::Secret;

use super::panes::PaneEntry;
use super::{Ending, NodeHandle, RegistryHandle, lock, mint_token};
use crate::serve::{ConnId, Outbound};

/// What one pane-directed [`marion_core::proto::Input`] asks of its pane, once the variant that
/// carries it has been read off the frame.
#[derive(Clone, Copy)]
enum PaneDelivery<'a> {
    LegacyWrite(&'a [u8]),
    OpaqueWrite(&'a [u8]),
    Resize { cols: u16, rows: u16 },
}

impl RegistryHandle {
    /// §2's inbound notifications, delivered.
    ///
    /// **Both are gated on the write lease, including the resize**, and that is not over-caution.
    /// A resize is not a private view setting: it is `TIOCSWINSZ` on the one master, so a
    /// read-only attacher that could resize would reflow the pane of the operator who *is* typing,
    /// from under them, with no way for either to tell where it came from. §5.3 gives a node one
    /// writer; the geometry is part of what that means.
    ///
    /// Legacy refusals remain silent for compatibility: there is no response envelope, and the
    /// client was already told at `node/attach` that it may not type. Pane-v1 opaque input is
    /// stronger: it is accepted only from that connection's live negotiated slot, and any refusal
    /// visibly ends that exact socket before a terminal `End` can claim success.
    pub(super) fn deliver_input(&self, conn: ConnId, input: &marion_core::proto::Input) {
        self.deliver_input_with_out(conn, input, None);
    }

    /// **The pane-v1 replay handshake**, completed by read-only viewers too.
    fn deliver_pane_ready(&self, conn: ConnId, ready: &marion_core::proto::NodePaneReadyV1) {
        // Read-only viewers complete this handshake too, so it is deliberately independent of
        // the keyboard lease. Registry expiry runs first: a token cannot revive a Completed
        // host at or beyond its exact cache deadline.
        self.prune_completed_panes();
        let host = {
            let panes = lock(&self.panes);
            panes
                .hosts
                .get(&ready.agent_id)
                .and_then(PaneEntry::replay_host)
                .cloned()
        };
        if let Some(host) = host {
            host.pane_ready(conn, &ready.token, ready.cut);
        }
    }

    /// **One opaque keystroke's admitted delivery**: selection and admission under the
    /// registry lock, master I/O after it, and the slot outbound failed on either refusal.
    fn deliver_opaque_input(
        &self,
        conn: ConnId,
        input: &marion_core::proto::Input,
        id: &AgentId,
        bytes: &[u8],
        wire_out: Option<&Outbound>,
    ) {
        // Selection, pane-v1 generation validation, and control admission are one registry
        // transaction. The admission owns the exact slot outbound; durability and master I/O
        // happen only after the global Panes lock is released.
        let selected = {
            let panes = lock(&self.panes);
            let lease = panes.lease(conn, id).ok_or_else(|| {
                "opaque pane input requires this connection's live write lease".to_string()
            });
            lease.and_then(|lease| {
                let host = panes
                    .hosts
                    .get(id)
                    .and_then(PaneEntry::live_host)
                    .cloned()
                    .ok_or_else(|| "opaque pane input requires a live pane".to_string())?;
                let admission = host
                    .admit_opaque_input(&lease, conn)
                    .map_err(|error| error.to_string())?;
                Ok((host, admission))
            })
        };
        let (host, admission) = match selected {
            Ok(selected) => selected,
            Err(error) => {
                if let Some(out) = wire_out {
                    out.fail(crate::serve::Departure::PaneInputFailed {
                        agent_id: id.0.clone(),
                        error: error.clone(),
                    });
                }
                eprintln!("marion: {} on node {}: {error}", input.method(), id.0);
                return;
            }
        };
        #[cfg(test)]
        if let Some(hook) = lock(&self.pane_delivery_hook).take() {
            hook();
        }
        if let Err(error) = host.write_opaque_input_admitted(admission, bytes) {
            // The admission fails its authoritative slot outbound before releasing the input
            // delivery barrier, including error and unwind paths.
            eprintln!("marion: {} on node {}: {error}", input.method(), id.0);
        }
    }

    /// **Which pane and what it is being asked to do**, read off the frame alone.
    fn classify_pane_delivery(input: &marion_core::proto::Input) -> (&AgentId, PaneDelivery<'_>) {
        match input {
            marion_core::proto::Input::NodePtyWrite {
                agent_id, bytes, ..
            } => (agent_id, PaneDelivery::LegacyWrite(bytes.as_bytes())),
            marion_core::proto::Input::NodePaneWrite(params) => (
                &params.agent_id,
                PaneDelivery::OpaqueWrite(params.bytes.as_bytes()),
            ),
            marion_core::proto::Input::NodeResize {
                agent_id,
                cols,
                rows,
            } => (
                agent_id,
                PaneDelivery::Resize {
                    cols: *cols,
                    rows: *rows,
                },
            ),
            marion_core::proto::Input::NodePaneReady(_) => unreachable!("handled above"),
        }
    }

    /// **A leased legacy write or resize**: the lease and host are taken out from under
    /// the registry lock in one look, and the lock is released before the write.
    fn deliver_leased_input(
        &self,
        conn: ConnId,
        input: &marion_core::proto::Input,
        id: &AgentId,
        delivery: PaneDelivery<'_>,
    ) {
        // Both taken out from under the lock in one look, and the lock released before the write:
        // see `Panes::leases`. A harness that has stopped reading its stdin must stall one attach,
        // never the supervisor.
        let (host, lease) = {
            let panes = lock(&self.panes);
            let Some(lease) = panes.lease(conn, id) else {
                return;
            };
            match panes.hosts.get(id).and_then(PaneEntry::live_host) {
                Some(h) => (Arc::clone(h), lease),
                None => return,
            }
        };
        #[cfg(test)]
        if let Some(hook) = lock(&self.pane_delivery_hook).take() {
            hook();
        }
        let outcome = match delivery {
            PaneDelivery::LegacyWrite(bytes) => host.write_input(&lease, bytes),
            PaneDelivery::OpaqueWrite(_) => unreachable!("opaque input returned above"),
            PaneDelivery::Resize { cols, rows } => {
                host.resize(crate::pty::WinSize::new(cols, rows))
            }
        };
        if let Err(e) = outcome {
            eprintln!("marion: {} on node {}: {e}", input.method(), id.0);
        }
    }

    pub(super) fn deliver_input_with_out(
        &self,
        conn: ConnId,
        input: &marion_core::proto::Input,
        wire_out: Option<&Outbound>,
    ) {
        if let marion_core::proto::Input::NodePaneReady(ready) = input {
            self.deliver_pane_ready(conn, ready);
            return;
        }
        let (id, delivery) = Self::classify_pane_delivery(input);
        if let PaneDelivery::OpaqueWrite(bytes) = delivery {
            self.deliver_opaque_input(conn, input, id, bytes, wire_out);
            return;
        }
        self.deliver_leased_input(conn, input, id, delivery);
    }

    /// How many node streams this supervisor is following on behalf of a client.
    pub fn attachments(&self) -> usize {
        lock(&self.shared).attached.len()
    }

    /// **The subscribe leg's engine**: advance every attached cursor and push what it found.
    ///
    /// Returns how many events were read — per event, not per delivery, for the reason
    /// [`Self::flush`] gives.
    ///
    /// One `open` and one `stat` per attachment per call, which is [`EventReader::poll`](crate::events::EventReader::poll)'s stated
    /// cost when there is nothing new, and nothing new is the common case. §11 item 27 is the entry
    /// that makes this cheaper; nothing here depends on it landing.
    pub fn pump_attached(&self) -> usize {
        let mut g = lock(&self.shared);
        let mut read = 0usize;
        g.attached.retain_mut(|a| {
            let mut fresh = Vec::new();
            read += a.reader.poll(&mut fresh);
            deliver_events(&a.out, &a.agent_id, &fresh)
        });
        read
    }

    /// **Take ownership of a node the instant it has an identity**, and mint it §5.4's capability.
    ///
    /// Called from [`crate::run::SpawnObserver::identified`], which fires with the `SpawnIntent`
    /// already durable and no side effect yet taken. The token returned is written into the node's
    /// MCP declaration a few lines later, so this is the last moment it can be decided.
    ///
    /// `repo` is the tree this node lives in, remembered so this node's own children can branch
    /// from it — see [`NodeHandle::repo`]. It is the caller's `repo`, inherited rather than
    /// re-derived, because "which tree" is a fact about the subtree and not about the spawn.
    ///
    /// `pub(crate)` rather than private because it is also the whole of what a test needs to put a
    /// node in this table — and a test that reached in through a back door would be asserting
    /// against a binding production does not make.
    pub(crate) fn claim(
        &self,
        agent_id: &AgentId,
        task_id: Option<TaskId>,
        repo: PathBuf,
    ) -> Secret {
        let token = mint_token();
        lock(&self.nodes).insert(
            agent_id.clone(),
            NodeHandle {
                task_id,
                token: token.clone(),
                repo,
                pid: None,
                pgid: None,
                started_at: None,
                join: None,
                outcome: None,
                ending: Ending::Running,
                process_gone: false,
                escalated: false,
            },
        );
        self.inboxes.open(agent_id);
        token
    }
}

/// Send a run of a node's events to one client. `false` means the connection is finished.
///
/// Every field a client needs to place the event in the stream comes off the event itself; nothing
/// is derived from the moment of delivery. In particular `ts` is the writer's, not this process's
/// clock — a replayed event that claimed to have happened when it was replayed would make the
/// detached window look like it never existed.
pub(super) fn deliver_events(
    out: &Outbound,
    agent_id: &AgentId,
    events: &[marion_core::event::Event],
) -> bool {
    for e in events {
        let payload = serde_json::to_value(&e.payload).unwrap_or_else(|err| {
            // Unreachable for a payload this process just read out of an encoded line, and stated
            // rather than defaulted to `null`: a client must be able to tell "the node said
            // nothing" from "marion could not re-encode what the node said".
            serde_json::json!({ "marion_unencodable": err.to_string() })
        });
        let ok = out.notify(Event::NodeEvent {
            agent_id: agent_id.clone(),
            agent_seq: e.agent_seq,
            ts: e.ts,
            provenance: e.provenance.clone(),
            src_seq: e.src_seq.clone(),
            payload,
        });
        if !ok {
            return false;
        }
    }
    true
}
