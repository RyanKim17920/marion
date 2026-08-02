#!/usr/bin/env python3
"""Canned OpenAI-Responses provider for spike S7 (process-group containment).

Turn 1 emits a `custom_tool_call` to codex's code-mode `exec` tool whose
JavaScript shells out via `tools.exec_command(...)` to a script that starts a
durable `sleep` child *and* a backgrounded grandchild, recording their pids.

Turn 2 deliberately BLOCKS for S7_HOLD_SECS before answering. That keeps
`codex exec` alive while the probe harness captures `ps` and then issues
`killpg` against the process group the parent created. Without the hold, codex
would exit on its own and the kill would prove nothing.

No model is called and no API key is used.
"""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn

REQLOG = os.getenv("S7_REQLOG", "/tmp/s7-requests.jsonl")
PORT = int(os.getenv("S7_PORT", "8098"))
SPAWN_SH = os.environ["S7_SPAWN_SH"]        # case A script (orphan)
PIDFILE = os.environ["S7_PIDFILE"]          # case A pidfile
RUNAWAY_SH = os.environ["S7_RUNAWAY_SH"]    # case B script (still running)
RUNAWAY_PIDFILE = os.environ["S7_RUNAWAY_PIDFILE"]
HOLD_SECS = int(os.getenv("S7_HOLD_SECS", "120"))

_turn = {"n": 0}


def sse(events):
    out = []
    for ev in events:
        out.append("event: " + ev["type"] + "\n")
        out.append("data: " + json.dumps(ev) + "\n\n")
    return "".join(out).encode()


def spawn_child_events():
    """custom_tool_call -> exec, whose JS runs spawn.sh via exec_command."""
    js = (
        '// @exec: {"yield_time_ms": 20000, "max_output_tokens": 2000}\n'
        "// case A: command completes, leaving orphaned background sleepers\n"
        "const a = await tools.exec_command({\n"
        "  cmd: %s, shell: \"/bin/sh\", login: false,\n"
        "  yield_time_ms: 5000, max_output_tokens: 2000 });\n"
        "text(JSON.stringify({caseA: a}));\n"
        "// case B: command is STILL RUNNING when exec yields (session_id set)\n"
        "const b = await tools.exec_command({\n"
        "  cmd: %s, shell: \"/bin/sh\", login: false,\n"
        "  yield_time_ms: 250, max_output_tokens: 2000 });\n"
        "text(JSON.stringify({caseB: b}));\n"
    ) % (json.dumps("/bin/sh %s %s" % (SPAWN_SH, PIDFILE)),
         json.dumps("/bin/sh %s %s" % (RUNAWAY_SH, RUNAWAY_PIDFILE)))
    item = {"type": "custom_tool_call", "id": "ct_s7", "call_id": "call_s7_1",
            "name": "exec", "input": js}
    return [
        {"type": "response.created", "response": {"id": "resp_s7_1"}},
        {"type": "response.output_item.done", "item": item},
        {"type": "response.completed", "response": {"id": "resp_s7_1", "output": [item]}},
    ]


def final_text_events(text):
    item = {"type": "message", "id": "msg_s7", "role": "assistant",
            "content": [{"type": "output_text", "text": text}]}
    return [
        {"type": "response.created", "response": {"id": "resp_s7_2"}},
        {"type": "response.output_item.done", "item": item},
        {"type": "response.completed", "response": {"id": "resp_s7_2", "output": [item]}},
    ]


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
        _turn["n"] += 1
        turn = _turn["n"]
        with open(REQLOG, "a") as fh:
            fh.write(json.dumps({"turn": turn, "path": self.path, "body": body}) + "\n")

        if turn == 1:
            events = spawn_child_events()
        else:
            # Hold the connection open so codex stays alive for the kill test.
            sys.stderr.write("s7: holding turn %d for %ds\n" % (turn, HOLD_SECS))
            sys.stderr.flush()
            time.sleep(HOLD_SECS)
            events = final_text_events("done")

        payload = sse(events)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class Server(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    srv = Server(("127.0.0.1", PORT), Handler)
    sys.stderr.write("s7 canned provider on 127.0.0.1:%d hold=%ds\n" % (PORT, HOLD_SECS))
    sys.stderr.flush()
    srv.serve_forever()
