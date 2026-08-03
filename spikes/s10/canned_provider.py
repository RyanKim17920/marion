#!/usr/bin/env python3
"""Canned Anthropic-Messages provider for spike S10 (SubagentStop live fire).

Drives a real `claude` (2.1.220) through a Task-tool subagent with **no model
call and no paid tokens**: `ANTHROPIC_BASE_URL` points here.

Turn routing is by request *content*, not by a counter, because Claude Code
issues concurrent side requests (session title, topic detection) on the same
endpoint:

  * a request whose last user turn contains PROBE_TOKEN  -> the **subagent**
  * a request that already carries a `tool_result` for our Task call -> the
    **root, after the subagent returned** -> finish
  * anything else -> the **root's first turn** -> emit `tool_use` for `Task`

The subagent replies with one word so it stops immediately; that stop is the
event under test. If S10_SUBAGENT_TURNS > 1 the subagent is served a second
short reply, which is what a `{"decision":"block"}` re-prompt needs.
"""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn

REQLOG = os.environ["S10_REQLOG"]
PORT = int(os.getenv("S10_PORT", "8110"))
PROBE_TOKEN = os.getenv("S10_PROBE_TOKEN", "MARION_S10_SUBAGENT_PROBE")
SUBAGENT_TYPE = os.getenv("S10_SUBAGENT_TYPE", "general-purpose")


def sse(event, data):
    return "event: %s\ndata: %s\n\n" % (event, json.dumps(data))


def text_turn(text, mid="msg_s10_text"):
    return "".join([
        sse("message_start", {"type": "message_start", "message": {
            "id": mid, "type": "message", "role": "assistant", "model": "canned",
            "content": [], "stop_reason": None,
            "usage": {"input_tokens": 0, "output_tokens": 0}}}),
        sse("content_block_start", {"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        sse("content_block_delta", {"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": text}}),
        sse("content_block_stop", {"type": "content_block_stop", "index": 0}),
        sse("message_delta", {"type": "message_delta",
            "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 0}}),
        sse("message_stop", {"type": "message_stop"}),
    ])


def tool_use_turn(name, tool_use_id, tool_input, mid="msg_s10_tool"):
    return "".join([
        sse("message_start", {"type": "message_start", "message": {
            "id": mid, "type": "message", "role": "assistant", "model": "canned",
            "content": [], "stop_reason": None,
            "usage": {"input_tokens": 0, "output_tokens": 0}}}),
        sse("content_block_start", {"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": tool_use_id,
                              "name": name, "input": {}}}),
        sse("content_block_delta", {"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                      "partial_json": json.dumps(tool_input)}}),
        sse("content_block_stop", {"type": "content_block_stop", "index": 0}),
        sse("message_delta", {"type": "message_delta",
            "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 0}}),
        sse("message_stop", {"type": "message_stop"}),
    ])


# In 2.1.220 the subagent tool is advertised to the model as **`Agent`**, though
# `--tools` and `system/init` both call it `Task`. Emitting a `tool_use` named
# either one works (measured -- see the fixture README); the name that matters is
# the one to *match on* when routing, and that is `Agent`.
TASK_TOOL = os.getenv("S10_TASK_TOOL", "Agent")
TASK_TOOL_NAMES = ("Agent", "Task")
TASK_INPUT = {
    "description": "s10 probe",
    "prompt": PROBE_TOKEN + ": reply with exactly the word ok and nothing else.",
    "subagent_type": SUBAGENT_TYPE,
    # synchronous: background subagents let the root finish first, and the root
    # exiting would tear the child down before it could stop.
    "run_in_background": False,
}
TOOL_USE_ID = "toolu_s10_task_1"


def flatten(body):
    """All text in the request's messages, as one string."""
    out = []
    for m in body.get("messages", []):
        c = m.get("content")
        if isinstance(c, str):
            out.append(c)
        elif isinstance(c, list):
            for b in c:
                if not isinstance(b, dict):
                    continue
                if "text" in b and isinstance(b["text"], str):
                    out.append(b["text"])
                cc = b.get("content")
                if isinstance(cc, str):
                    out.append(cc)
                elif isinstance(cc, list):
                    for d in cc:
                        if isinstance(d, dict) and isinstance(d.get("text"), str):
                            out.append(d["text"])
    return "\n".join(out)


def has_tool_result_for(body, tuid):
    for m in body.get("messages", []):
        c = m.get("content")
        if isinstance(c, list):
            for b in c:
                if isinstance(b, dict) and b.get("type") == "tool_result" \
                        and b.get("tool_use_id") == tuid:
                    return True
    return False


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(n)
        try:
            body = json.loads(raw)
        except Exception:
            body = {"_unparsed": raw.decode("utf-8", "replace")}

        text = flatten(body)
        tool_names = [t.get("name") for t in body.get("tools", []) or []]
        is_sub = PROBE_TOKEN in text
        did_task = has_tool_result_for(body, TOOL_USE_ID)

        if is_sub and "Stop hook feedback" in text:
            # only reachable if the block genuinely re-prompted the subagent
            kind = "subagent-reprompted"
            payload = text_turn("ok, i saw the stop hook feedback", "msg_s10_sub2")
        elif is_sub:
            kind = "subagent"
            payload = text_turn("ok", "msg_s10_sub")
        elif did_task:
            kind = "root-final"
            payload = text_turn("done", "msg_s10_root2")
        elif any(t in tool_names for t in TASK_TOOL_NAMES):
            kind = "root-first"
            payload = tool_use_turn(TASK_TOOL, TOOL_USE_ID, TASK_INPUT)
        else:
            # side request (session title, topic detection, ...) -- no Task tool
            kind = "side"
            payload = text_turn('{"title":"s10 probe","isNewTopic":false}',
                                "msg_s10_side")

        with open(REQLOG, "a") as fh:
            fh.write(json.dumps({
                "path": self.path, "kind": kind,
                "tool_names": tool_names, "body": body}) + "\n")
        sys.stderr.write("s10 %s %s\n" % (self.path, kind))
        sys.stderr.flush()

        enc = payload.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(enc)))
        self.end_headers()
        self.wfile.write(enc)


class Server(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    srv = Server(("127.0.0.1", PORT), Handler)
    sys.stderr.write("s10 canned provider on 127.0.0.1:%d\n" % PORT)
    sys.stderr.flush()
    srv.serve_forever()
