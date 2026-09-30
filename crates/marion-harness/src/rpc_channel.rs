//! **An id-correlated JSON-RPC thread channel, as row data** — [`crate::surfaces::TypedKind::AppServer`].
//!
//! Some harnesses serve a long-lived stdio JSON-RPC server that holds a *thread* (the session) and
//! runs *turns* inside it: every request carries an id its answer echoes, the server sends
//! notifications for what a turn does, and it sends requests of its own (approvals) that marion
//! must answer. `codex app-server` is the first (S36, `tests/fixtures/app-server-0.155.1/`). What
//! differs between two such servers is vocabulary: what the handshake, the thread and turn methods
//! are called, which fields they take, which notifications open and close a turn or say marion's
//! MCP server is ready, and what marion answers each server request with. So the vocabulary is an
//! [`RpcChannel`] on the row ([`crate::spec::Surfaces::AppServer`]) and one driver speaks every
//! channel (`marion-supervisor`'s `app_server`, over the id-correlated driver it shares with ACP).
//! A second harness with a server like this is a second row, never a second driver.

use serde_json::{Map, Value, json};

use crate::grammar::{Cond, frame_matches};

/// A text at `ptr` in `v`, numbers and booleans rendered, as [`crate::grammar`] compares values.
fn text_at(v: &Value, ptr: &str) -> Option<String> {
    match v.pointer(ptr)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The notification that says how one MCP server's startup went — the gate before the first turn.
///
/// S36 P3: a thread's first turn does not wait for marion's server beyond ~1 s; a request that
/// goes out earlier carries none of marion's tools. So the driver waits for this, never a sleep.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadyGate {
    /// Frames that report a server's startup state.
    pub at: &'static [Cond],
    /// The server the report is about.
    pub server: &'static str,
    /// Its state.
    pub status: &'static str,
    /// The state that opens the gate.
    pub ready: &'static str,
    /// States that close it for good.
    pub failed: &'static [&'static str],
    /// The server's own words on a failure.
    pub error: &'static str,
}

/// What a [`ReadyGate`] read off one frame, for the server it was asked about.
#[derive(Debug, PartialEq, Eq)]
pub enum Startup {
    Ready,
    Failed(String),
}

/// The turn half of the vocabulary: requests marion sends, and frames it reads a turn's life off.
#[derive(Debug, PartialEq, Eq)]
pub struct Turns {
    /// A new turn on an idle thread.
    pub start: &'static str,
    /// A message folded into the running turn (`MidTurn::Fold`).
    pub steer: &'static str,
    /// Ends the running turn early.
    pub interrupt: &'static str,
    /// The field every turn request names the thread in.
    pub thread: &'static str,
    /// The field the message rides in, as a list of one text item.
    pub input: &'static str,
    /// The text item: `(kind field, kind, text field)`.
    pub text_item: (&'static str, &'static str, &'static str),
    /// The field a steer names the turn it expects to be running in.
    pub expected: &'static str,
    /// The field an interrupt names the turn to end in.
    pub turn: &'static str,
    /// Frames that open a turn.
    pub started: &'static [Cond],
    /// Frames that close one.
    pub ended: &'static [Cond],
    /// The turn's id, in a frame that opens or closes it.
    pub turn_id: &'static str,
    /// The turn's id in the answer to a start or a steer.
    pub answered: &'static [&'static str],
    /// Frames that start a process for a turn — what marion kills itself after an interrupt.
    pub process: &'static [Cond],
    /// Frames that end one, naming the same pid.
    pub process_ended: &'static [Cond],
    /// The process's pid, in such a frame.
    pub pid: &'static str,
}

/// A server request marion answers with a fixed result.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub method: &'static str,
    /// The result, as JSON text.
    pub result: &'static str,
}

/// **A harness's JSON-RPC thread channel**: the handshake, the thread, the gate, the turns, and
/// marion's answers to the server's own requests.
///
/// Every field was read off a measurement; `note` names it.
#[derive(Debug, PartialEq, Eq)]
pub struct RpcChannel {
    /// The handshake request.
    pub initialize: &'static str,
    /// The notification sent after the handshake's answer, where the protocol has one.
    pub initialized: Option<&'static str>,
    /// Notification methods the connection asks the server not to send: marion reads none of them,
    /// and each costs a frame on every token.
    pub opt_out: &'static [&'static str],
    /// Opens a fresh thread.
    pub open: &'static str,
    /// Reopens a thread by id (a resume).
    pub resume: &'static str,
    /// The field [`Self::resume`] names the thread in.
    pub resume_id: &'static str,
    /// The field both name the working directory in.
    pub cwd: &'static str,
    /// Fields both carry verbatim, as `(name, JSON text)`.
    pub open_fields: &'static [(&'static str, &'static str)],
    /// What a **read-only** launch (a reviewer's) carries in place of the [`Self::open_fields`] of
    /// the same name. Where the thread's own request states the sandbox it beats every other
    /// switch (codex S36 P3), so the row's argv switch alone would be overridden here.
    pub read_only_fields: &'static [(&'static str, &'static str)],
    /// The thread id, in the answer to either.
    pub thread_id: &'static str,
    /// The gate before the first turn, where marion's MCP server is declared.
    pub ready: Option<ReadyGate>,
    pub turns: Turns,
    /// What marion answers each server request it knows with. Every other request is answered with
    /// a JSON-RPC error naming it — answered, never dropped, since an unanswered request stalls
    /// the turn.
    pub answers: &'static [Answer],
    /// **Mandatory.** The measurement behind this vocabulary.
    pub note: &'static str,
}

/// An object of `pairs`, in order — the vocabulary's field names are data, not literals.
fn obj<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// A JSON-RPC request: `{method, id, params}`, with no `jsonrpc` member (S36 P2: the server writes
/// none, and reads frames without one).
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"method": method, "id": id, "params": params})
}

impl RpcChannel {
    /// The handshake, naming marion as the client.
    pub fn initialize_request(&self, id: u64, version: &str) -> Value {
        request(
            id,
            self.initialize,
            json!({
                "clientInfo": {"name": "marion", "title": null, "version": version},
                "capabilities": {
                    "experimentalApi": false,
                    "optOutNotificationMethods": self.opt_out,
                },
            }),
        )
    }

    /// The notification that follows the handshake's answer, where there is one.
    pub fn initialized_notification(&self) -> Option<Value> {
        self.initialized.map(|m| json!({"method": m}))
    }

    fn open_params(
        &self,
        cwd: &str,
        read_only: bool,
        replaced: &[(&str, &str)],
    ) -> Map<String, Value> {
        let mut p = Map::new();
        p.insert(self.cwd.into(), Value::String(cwd.into()));
        let over: &[(&str, &str)] = if read_only {
            self.read_only_fields
        } else {
            &[]
        };
        for (k, v) in self.open_fields.iter().chain(over).chain(replaced) {
            let v = serde_json::from_str(v).expect("open_fields are JSON text");
            p.insert((*k).into(), v);
        }
        p
    }

    /// The request that opens this launch's thread: [`Self::open`], or [`Self::resume`] of
    /// `resume`'s thread — with [`Self::read_only_fields`] on a read-only launch, and `replaced`
    /// last, over both: the fields that switch the harness's own sandbox off where marion's
    /// replaces it ([`crate::os_sandbox::replaced_fields`]).
    pub fn opening(
        &self,
        id: u64,
        cwd: &str,
        resume: Option<&str>,
        read_only: bool,
        replaced: &[(&str, &str)],
    ) -> Value {
        let mut p = self.open_params(cwd, read_only, replaced);
        let method = match resume {
            Some(thread) => {
                p.insert(self.resume_id.into(), Value::String(thread.into()));
                self.resume
            }
            None => self.open,
        };
        request(id, method, Value::Object(p))
    }

    /// Is `request` one of this channel's opening requests? `Some(resumed thread)` for a resume.
    pub fn opened_by(&self, request: &Value) -> Option<Option<String>> {
        match request.get("method").and_then(Value::as_str)? {
            m if m == self.open => Some(None),
            m if m == self.resume => Some(text_at(request, &format!("/params/{}", self.resume_id))),
            _ => None,
        }
    }

    /// The thread id in an opening request's answer.
    pub fn thread_of(&self, answer: &Value) -> Option<String> {
        text_at(answer, self.thread_id)
    }

    fn input(&self, text: &str) -> Value {
        let (kind_field, kind, text_field) = self.turns.text_item;
        Value::Array(vec![obj([
            (kind_field, Value::String(kind.into())),
            (text_field, Value::String(text.into())),
        ])])
    }

    /// `text` as a new turn on `thread`.
    pub fn turn_request(&self, id: u64, thread: &str, text: &str) -> Value {
        let t = &self.turns;
        request(
            id,
            t.start,
            obj([(t.thread, thread.into()), (t.input, self.input(text))]),
        )
    }

    /// `text` folded into `turn`, the one running on `thread`.
    pub fn steer_request(&self, id: u64, thread: &str, turn: &str, text: &str) -> Value {
        let t = &self.turns;
        request(
            id,
            t.steer,
            obj([
                (t.thread, thread.into()),
                (t.expected, turn.into()),
                (t.input, self.input(text)),
            ]),
        )
    }

    /// Ends `turn` on `thread`.
    pub fn interrupt_request(&self, id: u64, thread: &str, turn: &str) -> Value {
        let t = &self.turns;
        request(
            id,
            t.interrupt,
            obj([(t.thread, thread.into()), (t.turn, turn.into())]),
        )
    }

    /// The turn `frame` opens, if it opens one.
    pub fn opens_turn(&self, frame: &Value) -> Option<String> {
        frame_matches(frame, self.turns.started)
            .then(|| text_at(frame, self.turns.turn_id))
            .flatten()
    }

    /// The turn `frame` closes, if it closes one.
    pub fn closes_turn(&self, frame: &Value) -> Option<String> {
        frame_matches(frame, self.turns.ended)
            .then(|| text_at(frame, self.turns.turn_id))
            .flatten()
    }

    /// The turn a start or steer answer names.
    pub fn answered_turn(&self, answer: &Value) -> Option<String> {
        self.turns.answered.iter().find_map(|p| text_at(answer, p))
    }

    /// The pid of a process `frame` says a turn started.
    pub fn process_of(&self, frame: &Value) -> Option<i32> {
        frame_matches(frame, self.turns.process)
            .then(|| text_at(frame, self.turns.pid)?.parse().ok())
            .flatten()
    }

    /// The pid of a process `frame` says has ended.
    pub fn process_ended(&self, frame: &Value) -> Option<i32> {
        frame_matches(frame, self.turns.process_ended)
            .then(|| text_at(frame, self.turns.pid)?.parse().ok())
            .flatten()
    }

    /// How `server`'s startup went, if `frame` says.
    pub fn startup(&self, frame: &Value, server: &str) -> Option<Startup> {
        let g = self.ready.as_ref()?;
        if !frame_matches(frame, g.at) || text_at(frame, g.server).as_deref() != Some(server) {
            return None;
        }
        let status = text_at(frame, g.status)?;
        if status == g.ready {
            return Some(Startup::Ready);
        }
        g.failed.contains(&status.as_str()).then(|| {
            Startup::Failed(text_at(frame, g.error).unwrap_or_else(|| format!("status `{status}`")))
        })
    }

    /// marion's answer to a server request: the row's result for a method it names, else an error
    /// naming the method.
    pub fn answer(&self, request: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match self.answers.iter().find(|a| a.method == method) {
            Some(a) => json!({
                "id": id,
                "result": serde_json::from_str::<Value>(a.result).expect("answers are JSON text"),
            }),
            None => json!({
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("marion does not answer `{method}`"),
                },
            }),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Every JSON text in a channel parses, and every answer is an object.
    pub fn assert_well_formed(c: &RpcChannel) {
        assert!(!c.note.is_empty());
        for (k, v) in c.open_fields.iter().chain(c.read_only_fields) {
            serde_json::from_str::<Value>(v).unwrap_or_else(|e| panic!("open field {k}: {e}"));
        }
        for a in c.answers {
            let v: Value = serde_json::from_str(a.result)
                .unwrap_or_else(|e| panic!("answer to {}: {e}", a.method));
            assert!(v.is_object(), "answer to {} is an object", a.method);
        }
    }
}
