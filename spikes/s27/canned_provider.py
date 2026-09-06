#!/usr/bin/env python3
"""Canned **OpenAI-compatible** provider for spike S27 (Cline CLI at $0.00).

Adapted from spikes/s23/canned_provider.py (env prefix S23_ -> S27_). Cline 3.0.61's
`openai-compatible` provider is `@ai-sdk/openai-compatible` under the hood, so the wire is
`POST {baseUrl}/chat/completions` with SSE `chat.completion.chunk` deltas, and tool definitions
travel as OpenAI `tools[]` -- **not** as a prose/XML system prompt. (That was measured, not
assumed: every S27 run logged `tools[]` with 26 function entries, `marion__report` among them,
and the 4.4 KB system prompt mentions neither "mcp" nor the tool; the installed bundle contains
no `use_mcp_tool` string at all.)

Turn dispatch is on request *shape*, never on a counter:

  * a request carrying a `role: "tool"` message  -> the turn **after** the MCP call came back
    -> one short text, `finish_reason: "stop"`.
  * a request whose `tools[]` contains S27_TOOL   -> the real turn -> a streamed `tool_calls`
    delta naming S27_TOOL. The name is looked up in the array the harness itself sent; if it is
    absent the stub refuses to emit a call (`kind: "turn-no-marion-tool"`).
  * anything else                                -> a side stub (title/summary style requests).

S27_FAIL_STATUS=500 makes every POST answer that status with a JSON error body instead, for the
provider-failure capture.

Env: S27_PORT, S27_REQLOG, S27_TOOL (default `marion__report`, the spelling Cline gave the
model on the wire), S27_MODEL, S27_NARRATIVE, S27_FAIL_STATUS.

Every request body is logged verbatim except the `Authorization` header, which is recorded as a
redaction marker only.
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn

PORT = int(os.getenv("S27_PORT", "8127"))
REQLOG = os.getenv("S27_REQLOG", "")
TOOL = os.getenv("S27_TOOL", "marion__report")
NARRATIVE = os.getenv("S27_NARRATIVE", "hello from cline under a canned provider")
MODEL = os.getenv("S27_MODEL", "canned-1")
FAIL_STATUS = int(os.getenv("S27_FAIL_STATUS", "0") or 0)

_lock = threading.Lock()
_seq = {"n": 0}

SECRET_HEADERS = ("authorization", "x-api-key", "api-key", "proxy-authorization")


def _log(rec):
    if not REQLOG:
        return
    with _lock:
        with open(REQLOG, "a") as fh:
            fh.write(json.dumps(rec) + "\n")


def redact_headers(headers):
    out = {}
    for k, v in headers.items():
        out[k] = "<redacted:%d chars>" % len(v) if k.lower() in SECRET_HEADERS else v
    return out


def chunk(delta, finish=None, cid="chatcmpl-s27"):
    return "data: %s\n\n" % json.dumps({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })


def text_turn(text, cid="chatcmpl-s27-text"):
    return "".join([
        chunk({"role": "assistant", "content": ""}, cid=cid),
        chunk({"content": text}, cid=cid),
        chunk({}, finish="stop", cid=cid),
        "data: [DONE]\n\n",
    ])


def tool_call_turn(name, args, call_id="call_s27_marion_1", cid="chatcmpl-s27-tool"):
    """Name on the opening delta with empty arguments, JSON on a later fragment -- the way a
    real provider streams it."""
    return "".join([
        chunk({"role": "assistant", "content": None}, cid=cid),
        chunk({"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                               "function": {"name": name, "arguments": ""}}]}, cid=cid),
        chunk({"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                               "function": {"arguments": json.dumps(args)}}]}, cid=cid),
        chunk({}, finish="tool_calls", cid=cid),
        "data: [DONE]\n\n",
    ])


def has_tool_role(body):
    return any(m.get("role") == "tool" for m in body.get("messages", []) or [])


def tool_names(body):
    out = []
    for t in body.get("tools", []) or []:
        fn = t.get("function") or {}
        n = fn.get("name") or t.get("name")
        if n:
            out.append(n)
    return out


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _sse(self, payload):
        enc = payload.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(enc)))
        self.end_headers()
        self.wfile.write(enc)
        self.wfile.flush()

    def _json(self, obj, status=200):
        enc = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(enc)))
        self.end_headers()
        self.wfile.write(enc)

    def do_GET(self):
        _log({"path": self.path, "method": "GET", "kind": "get",
              "headers": redact_headers(self.headers)})
        if self.path.rstrip("/").endswith("models"):
            self._json({"object": "list", "data": [
                {"id": MODEL, "object": "model", "owned_by": "marion"}]})
        else:
            self._json({"ok": True})

    def do_POST(self):
        n = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            body = json.loads(raw)
        except Exception:
            body = {"_unparsed": raw.decode("utf-8", "replace")}

        with _lock:
            _seq["n"] += 1
            seq = _seq["n"]
        names = tool_names(body)

        if FAIL_STATUS:
            kind, payload = "fail-%d" % FAIL_STATUS, None
        elif has_tool_role(body):
            kind, payload = "turn-after-tool", text_turn("S27-OK", "chatcmpl-s27-after")
        elif TOOL in names:
            kind = "turn-with-marion-tool"
            payload = tool_call_turn(TOOL, {"narrative": NARRATIVE})
        elif names:
            kind = "turn-no-marion-tool"
            payload = text_turn("S27-NO-MARION-TOOL")
        else:
            kind, payload = "side-stub", text_turn("marion s27", "chatcmpl-s27-side")

        _log({"seq": seq, "path": self.path, "method": "POST", "kind": kind,
              "headers": redact_headers(self.headers),
              "tool_names": names, "body": body})
        sys.stderr.write("s27 %s #%d %s tools=%d\n" % (self.path, seq, kind, len(names)))
        sys.stderr.flush()
        try:
            if payload is None:
                self._json({"error": {"message": "canned failure", "type": "server_error",
                                      "code": "s27_canned_%d" % FAIL_STATUS}}, status=FAIL_STATUS)
            else:
                self._sse(payload)
        except (BrokenPipeError, ConnectionResetError, OSError) as e:
            sys.stderr.write("s27: response aborted by client: %r\n" % (e,))
            sys.stderr.flush()


class Server(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    srv = Server(("127.0.0.1", PORT), Handler)
    sys.stderr.write("s27 canned provider on 127.0.0.1:%d\n" % PORT)
    sys.stderr.flush()
    srv.serve_forever()
