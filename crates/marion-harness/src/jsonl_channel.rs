//! **A JSONL command channel, as row data** — [`crate::surfaces::TypedKind::JsonlRpc`].
//!
//! Some harnesses take commands as one JSON object per line on stdin and answer on stdout with the
//! same event frames their one-shot JSON mode prints, plus a reply per command: pi's `--mode rpc`
//! (S34, `tests/fixtures/s34-pi/pi-rpc-*`) is the first. What differs between two such harnesses is
//! only vocabulary: the field a command's kind is named in, what a prompt, a steer and an abort are
//! called, where the text goes, and which frames open and close a turn. So the vocabulary is a
//! [`JsonlChannel`] on the row ([`crate::spec::Surfaces::JsonlRpc`]) and one driver speaks every
//! channel (`marion-supervisor`'s `duplex::Dialect::Jsonl`). A second harness with a channel like
//! this is a second row, never a second driver.

use serde_json::{Map, Value};

use crate::grammar::{Cond, frame_matches};

/// One command, as data: literal string fields, plus the field the message text rides in.
#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    /// Fields every instance carries, verbatim — pi's `("type", "prompt")`.
    pub fields: &'static [(&'static str, &'static str)],
    /// The field the message text is written to, or `None` for a command with no text (`abort`).
    pub text: Option<&'static str>,
}

impl Command {
    /// The command as one JSON object: its fields, the text where it takes one, and the channel's
    /// correlation id where one is given. A text for a command that takes none is not written:
    /// the vocabulary decides the shape, never the caller.
    pub fn render(&self, id: Option<(&str, &str)>, text: &str) -> Value {
        let mut o = Map::new();
        if let Some((field, value)) = id {
            o.insert(field.into(), Value::String(value.into()));
        }
        for (k, v) in self.fields {
            o.insert((*k).into(), Value::String((*v).into()));
        }
        if let Some(field) = self.text {
            o.insert(field.into(), Value::String(text.into()));
        }
        Value::Object(o)
    }
}

/// **A harness's JSONL command channel**: what marion writes, and which frames it reads as a turn
/// opening, a turn closing, and a command being answered.
///
/// Every field was read off a measurement; `note` names it.
#[derive(Debug, PartialEq, Eq)]
pub struct JsonlChannel {
    /// The field a command's correlation id is written to, and echoed in on its reply.
    pub id: &'static str,
    /// Frames that answer a command, carrying its id in [`Self::id`].
    pub reply: &'static [Cond],
    /// Sent once, after marion's bridge has handed the harness its tool list and before the first
    /// prompt. Its reply proves the harness is reading commands, the role Claude Code's
    /// `initialize` control request plays on stream-json.
    pub handshake: Command,
    /// A new turn, for a node that is idle.
    pub prompt: Command,
    /// A message folded into a running turn (`MidTurn::Fold`). Also accepted by an idle harness
    /// as a new turn wherever the row measured that, which closes the race with a turn ending.
    pub steer: Command,
    /// Ends the running turn early. Written when a node's wall clock expires, before the kill.
    pub abort: Command,
    /// A frame that opens a turn.
    pub turn_started: &'static [Cond],
    /// A frame that closes one, and is the node's answer for it.
    pub turn_ended: &'static [Cond],
    /// **Mandatory.** The measurement behind this vocabulary.
    pub note: &'static str,
}

impl JsonlChannel {
    /// Is `frame` the reply to the command sent with `id`?
    pub fn is_reply_to(&self, frame: &Value, id: &str) -> bool {
        frame_matches(frame, self.reply) && frame.get(self.id).and_then(Value::as_str) == Some(id)
    }

    pub fn opens_turn(&self, frame: &Value) -> bool {
        frame_matches(frame, self.turn_started)
    }

    pub fn closes_turn(&self, frame: &Value) -> bool {
        frame_matches(frame, self.turn_ended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A vocabulary of the pi shape, for the driver's own tests and this module's.
    pub const TEST_CHANNEL: JsonlChannel = JsonlChannel {
        id: "id",
        reply: &[Cond::Eq("/type", "response")],
        handshake: Command {
            fields: &[("type", "get_state")],
            text: None,
        },
        prompt: Command {
            fields: &[("type", "prompt")],
            text: Some("message"),
        },
        steer: Command {
            fields: &[("type", "prompt"), ("streamingBehavior", "steer")],
            text: Some("message"),
        },
        abort: Command {
            fields: &[("type", "abort")],
            text: None,
        },
        turn_started: &[Cond::Eq("/type", "agent_start")],
        turn_ended: &[
            Cond::Eq("/type", "agent_end"),
            Cond::Eq("/willRetry", "false"),
        ],
        note: "test",
    };

    #[test]
    fn a_command_renders_its_fields_its_text_and_the_id_and_nothing_else() {
        let c = &TEST_CHANNEL;
        assert_eq!(
            c.steer.render(Some(("id", "m-1")), "look again"),
            json!({"id": "m-1", "type": "prompt", "streamingBehavior": "steer", "message": "look again"})
        );
        assert_eq!(
            c.abort.render(None, "ignored: abort takes no text"),
            json!({"type": "abort"})
        );
    }

    #[test]
    fn a_reply_is_matched_by_shape_and_id_and_a_retried_end_does_not_close_the_turn() {
        let c = &TEST_CHANNEL;
        let reply = json!({"id": "h", "type": "response", "command": "get_state", "success": true});
        assert!(c.is_reply_to(&reply, "h"));
        assert!(!c.is_reply_to(&reply, "other"));
        assert!(!c.is_reply_to(&json!({"id": "h", "type": "agent_start"}), "h"));
        assert!(c.opens_turn(&json!({"type": "agent_start"})));
        assert!(c.closes_turn(&json!({"type": "agent_end", "willRetry": false})));
        assert!(
            !c.closes_turn(&json!({"type": "agent_end", "willRetry": true})),
            "a run pi will retry is not the turn's end"
        );
    }
}
