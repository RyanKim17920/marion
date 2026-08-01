#!/usr/bin/env python3
"""Canned OpenAI-Responses provider for spike S6.

Serves POST /v1/responses with scripted SSE. Logs every inbound request body to
S6_REQLOG so the codex->provider wire (tool declarations, output schema) can be
read directly. No model is ever called, so runs are free and repeatable.

Script is selected by S6_SCRIPT:
  toolcall  - turn 1 emits an mcp_marion `report` call; turn 2 emits final text
  text      - single turn, plain final message (for --output-schema probing)
"""
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

REQLOG = os.getenv("S6_REQLOG", "/tmp/s6-requests.jsonl")
SCRIPT = os.getenv("S6_SCRIPT", "toolcall")
PORT = int(os.getenv("S6_PORT", "8099"))

_turn = {"n": 0}


def sse(events):
    out = []
    for ev in events:
        out.append("event: " + ev["type"] + "\n")
        out.append("data: " + json.dumps(ev) + "\n\n")
    return "".join(out).encode()


def exec_patch_events():
    """A custom-tool call to `exec` whose JS edits a file via apply_patch."""
    js = ("const patch = `*** Begin Patch\n"
          "*** Update File: a.rs\n"
          "@@\n"
          "-fn main(){}\n"
          "+fn main(){ println!(\\\"s6\\\"); }\n"
          "*** End Patch`;\n"
          "const r = await tools.apply_patch(patch);\n"
          "text(JSON.stringify(r));")
    item = {"type": "custom_tool_call", "id": "ct_s6", "call_id": "call_s6_p",
            "name": "exec", "input": js}
    return [
        {"type": "response.created", "response": {"id": "resp_s6_p"}},
        {"type": "response.output_item.done", "item": item},
        {"type": "response.completed", "response": {"id": "resp_s6_p", "output": [item]}},
    ]


def call_report_events():
    """A function_call to marion's report, in the namespace form codex expects."""
    item = {
        "type": "function_call",
        "id": "fc_s6_1",
        "call_id": "call_s6_1",
        "name": "report",
        "namespace": "mcp__marion",
        "arguments": json.dumps({"narrative": "s6 probe: reporting via MCP"}),
    }
    return [
        {"type": "response.created", "response": {"id": "resp_s6_1"}},
        {"type": "response.output_item.done", "item": item},
        {"type": "response.completed", "response": {"id": "resp_s6_1", "output": [item]}},
    ]


def final_text_events(text):
    item = {
        "type": "message",
        "id": "msg_s6",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}],
    }
    return [
        {"type": "response.created", "response": {"id": "resp_s6_2"}},
        {"type": "response.output_item.done", "item": item},
        {"type": "response.completed", "response": {"id": "resp_s6_2", "output": [item]}},
    ]


class Handler(BaseHTTPRequestHandler):
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
        with open(REQLOG, "a") as fh:
            fh.write(json.dumps({"turn": _turn["n"], "path": self.path, "body": body}) + "\n")

        if SCRIPT == "text":
            events = final_text_events(json.dumps({
                "narrative": "s6 probe: schema document via final message",
                "result_commits": [],
            }))
        elif SCRIPT == "patch":
            events = exec_patch_events() if _turn["n"] == 1 else final_text_events("patched")
        elif _turn["n"] == 1:
            events = call_report_events()
        else:
            events = final_text_events("done")

        payload = sse(events)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    srv = HTTPServer(("127.0.0.1", PORT), Handler)
    sys.stderr.write("canned provider on 127.0.0.1:%d script=%s\n" % (PORT, SCRIPT))
    srv.serve_forever()
