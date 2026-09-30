//! **Who each connection speaks for, and what that lets it call** — `session/hello` and the one
//! check every other call passes first.
//!
//! The socket is private to the operator's uid, but every process of that uid can `connect(2)` to
//! it, a node's own shell included. So the uid decides nothing here: a connection names its
//! principal once, with the operator's capability ([`crate::operator_key`]) or a node's own token,
//! and [`RegistryHandle::authorize`] holds every later call to what that principal may do. A
//! connection that has named nobody may call `session/hello` and nothing else — not even a read,
//! because a node's output can carry anything its model saw.
//!
//! A node may act about **itself and the nodes below it**: read the tree, attach to or steer or end
//! a descendant, spawn children and report a collected one as itself. Everything that acts for the
//! operator — a root, a resume, a quit, a doctor run, a permission answer — is the operator's alone.
//! Each method's own check still runs after this one (a node's `caller` is re-proved there): this
//! is the gate in front of it, not a replacement for it.

use marion_core::AgentId;
use marion_core::proto::params::SessionHelloParams;
use marion_core::proto::result::{SessionHelloResult, SessionPrincipal};
use marion_core::proto::{Call, RpcError};
use marion_core::secret::Secret;

use super::steer::is_strict_ancestor;
use super::{RegistryHandle, lock, unminted_token};
use crate::serve::ConnId;

impl RegistryHandle {
    /// `session/hello`: bind `conn` to the principal it proves, once.
    pub(super) fn session_hello(
        &self,
        conn: ConnId,
        p: &SessionHelloParams,
    ) -> Result<SessionHelloResult, RpcError> {
        if lock(&self.shared).principals.contains_key(&conn) {
            return Err(RpcError::refused(
                "session/hello",
                "this connection already said who it speaks for, and says it once: a connection \
                 that could change principal mid-stream would carry one principal's subscriptions \
                 and attachments into another's. Open a new connection to speak for someone else.",
                "§2",
            ));
        }
        let principal = match (&p.operator, &p.node) {
            (Some(key), None) => self.prove_operator(key)?,
            (None, Some(caller)) => match self.authenticate(caller) {
                Some(_) => SessionPrincipal::Node(caller.agent_id.clone()),
                None => return Err(unminted_token(&caller.agent_id)),
            },
            _ => {
                return Err(RpcError::refused(
                    "session/hello",
                    "a hello names exactly one principal: `operator` with the key in the state \
                     root's operator.key, or `node` with a node's own agent_id and token.",
                    "§2",
                ));
            }
        };
        lock(&self.shared)
            .principals
            .insert(conn, principal.clone());
        Ok(SessionHelloResult { principal })
    }

    fn prove_operator(&self, key: &Secret) -> Result<SessionPrincipal, RpcError> {
        match &self.operator_key {
            // `Secret`'s equality is constant-time, so a guess learns nothing from how long the
            // refusal took.
            Ok(ours) if ours == key => Ok(SessionPrincipal::Operator),
            Ok(_) => Err(RpcError::refused(
                "operator",
                "this is not the operator key in this supervisor's state root, so the connection \
                 speaks for nobody. marion's own clients read it from <state>/operator.key.",
                "§2",
            )),
            Err(why) => Err(RpcError::refused(
                "operator",
                format!(
                    "this supervisor holds no operator key, so no connection can speak for the operator: {why}"
                ),
                "§2",
            )),
        }
    }

    /// **The gate every call but `session/hello` passes first.** `Ok` means `conn`'s principal may
    /// make `call`; the method's own checks still follow.
    pub(super) fn authorize(&self, conn: ConnId, call: &Call) -> Result<(), RpcError> {
        let principal = lock(&self.shared).principals.get(&conn).cloned();
        match principal {
            None => Err(RpcError::refused(
                call.method().as_str(),
                "this connection has not said who it speaks for. Its first call must be \
                 `session/hello`, with the operator's key or a node's own token; the uid on the \
                 socket is shared by every process the operator runs and proves nothing.",
                "§2",
            )),
            Some(SessionPrincipal::Operator) => Ok(()),
            Some(SessionPrincipal::Node(me)) => self.node_may(&me, call),
        }
    }

    /// What node `me` may call: about itself and the nodes below it, never for the operator.
    fn node_may(&self, me: &AgentId, call: &Call) -> Result<(), RpcError> {
        let as_itself = |caller: Option<&AgentId>| match caller {
            Some(c) if c == me => Ok(()),
            Some(_) => Err(RpcError::refused(
                "caller",
                "a node's connection may name only that node as `caller`.",
                "§5.4",
            )),
            None => Err(operator_only(call)),
        };
        match call {
            Call::TreeSubscribe(_) | Call::NodeGet(_) | Call::NodeDetach(_) => Ok(()),
            Call::NodeAttach(p) if &p.agent_id == me || self.is_below(me, &p.agent_id) => Ok(()),
            Call::NodeAttach(_) => Err(not_below("attach to")),
            Call::NodeKill(p) if self.is_below(me, &p.agent_id) => Ok(()),
            Call::NodeKill(_) => Err(not_below("end")),
            Call::NodeCancel(p) => {
                as_itself(p.caller.as_ref().map(|c| &c.agent_id))?;
                if self.is_below(me, &p.agent_id) {
                    Ok(())
                } else {
                    Err(not_below("cancel"))
                }
            }
            Call::NodeSteer(p) => as_itself(p.caller.as_ref().map(|c| &c.agent_id)),
            Call::NodeCollected(p) => as_itself(Some(&p.caller.agent_id)),
            Call::AgentSpawn(p) => as_itself(p.caller.as_ref().map(|c| &c.agent_id)),
            _ => Err(operator_only(call)),
        }
    }

    fn is_below(&self, me: &AgentId, target: &AgentId) -> bool {
        self.live.refresh();
        self.live.read(|r| is_strict_ancestor(r.tree(), me, target))
    }
}

#[cfg(test)]
impl RegistryHandle {
    /// Make `conn` the operator's, as marion's own clients do first. Idempotent, unlike the wire's
    /// hello, so a helper may call it before every call it makes.
    pub(super) fn hello_as_operator(&self, conn: ConnId) {
        lock(&self.shared)
            .principals
            .insert(conn, SessionPrincipal::Operator);
    }

    /// The operator's `session/hello`, for a test that speaks the wire itself.
    pub(super) fn operator_hello(&self) -> SessionHelloParams {
        SessionHelloParams {
            operator: Some(self.operator_key.clone().expect("a key")),
            node: None,
        }
    }
}

fn operator_only(call: &Call) -> RpcError {
    RpcError::refused(
        call.method().as_str(),
        format!(
            "`{}` acts for the operator, and this connection speaks for a node. A node may read the \
             tree and act about itself and the nodes below it; starting a root, resuming, quitting \
             and answering for the operator are the operator's alone.",
            call.method().as_str()
        ),
        "§2, §5.4",
    )
}

fn not_below(verb: &str) -> RpcError {
    RpcError::refused(
        "agent_id",
        format!(
            "a node may {verb} only a node below it in the tree, and this one is not below the \
             node this connection speaks for."
        ),
        "§5.4",
    )
}
