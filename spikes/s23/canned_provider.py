#!/usr/bin/env python3
"""Canned **OpenAI-compatible** provider for spike S23 (`opencode acp` at $0.00).

The three existing canned providers (s10, s11, s16) all speak the *Anthropic
Messages* wire, because they drive `claude`. opencode's bundled
`@ai-sdk/openai-compatible` speaks `POST {baseURL}/chat/completions` with SSE
`chat.completion.chunk` deltas, so S23 needs its own. Zero cost: no model is
called and no real credential exists -- the `apiKey` in the config document is
the literal string `sk-marion-s23-canned`, minted here, and it is redacted out
of the request log.

Turn dispatch is on request *shape*, never on a counter, for the reason S11
round 16 recorded: opencode issues side requests (title/summary against
`small_model`, which marion pins to the same provider) concurrently with the
real turn, so a positional responder hands the scripted turn to the wrong
caller. The three shapes:

  * a request carrying a `role: "tool"` message  -> the turn **after** the MCP
    call came back -> one short text, `finish_reason: "stop"`.
  * a request whose `tools[]` contains `marion_report` -> the **real** turn ->
    a streamed `tool_calls` delta naming `marion_report`. This is the whole
    point of the spike: the canned model must *type the name*, because s14's
    finding is that a name the harness never saw on the wire is silently
    ignored.
  * anything else (no tools, or tools without marion's) -> a title/summary
    stub.

`marion_report` is not guessed here: S21 measured that spelling off a live
`opencode acp`, and opencode derives it as `<serverName>_<toolName>` from the
`session/new` declaration. The provider asserts nothing -- it looks the name up
in the `tools[]` array the agent itself sent, and refuses to emit a call if the
array does not contain it (`kind: "turn-no-marion-tool"`). A spike that emitted
the name blindly would pass with the bridge disconnected.

Every request body is logged verbatim except the `Authorization` header, which
is recorded as a redaction marker only.
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn

PORT = int(os.getenv("S23_PORT", "8123"))
REQLOG = os.getenv("S23_REQLOG", "")
TOOL = os.getenv("S23_TOOL", "marion_report")
NARRATIVE = os.getenv("S23_NARRATIVE", "hello from acp under a canned provider")
MODEL = os.getenv("S23_MODEL", "canned-1")

_lock = threading.Lock()
_seq = {"n": 0}

# Header values are never recorded. These are the ones whose *presence* is the
# finding (did opencode present the key marion minted, on marion's endpoint?),
# so the name is kept and the value replaced.
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


def chunk(delta, finish=None, cid="chatcmpl-s23"):
    return "data: %s\n\n" % json.dumps({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })


def text_turn(text, cid="chatcmpl-s23-text"):
    return "".join([
        chunk({"role": "assistant", "content": ""}, cid=cid),
        chunk({"content": text}, cid=cid),
        chunk({}, finish="stop", cid=cid),
        "data: [DONE]\n\n",
    ])


def tool_call_turn(name, args, call_id="call_s23_marion_1", cid="chatcmpl-s23-tool"):
    """Streamed the way a real provider streams it: the name arrives on the
    opening delta with empty arguments, the JSON arrives as a later fragment.
    A reader that took only the first delta would file a call with no
    arguments -- which is the same two-frame trap S21 found one layer up in
    ACP's `tool_call` / `tool_call_update` pair."""
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
        # `/v1/models` if opencode ever asks. It should not -- the config
        # declares the model inline -- so a hit here is itself a finding.
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

        if has_tool_role(body):
            kind, payload = "turn-after-tool", text_turn("S23-OK", "chatcmpl-s23-after")
        elif TOOL in names:
            kind = "turn-with-marion-tool"
            payload = tool_call_turn(TOOL, {"narrative": NARRATIVE})
        elif names:
            kind = "turn-no-marion-tool"
            payload = text_turn("S23-NO-MARION-TOOL")
        else:
            kind, payload = "side-stub", text_turn("marion s23", "chatcmpl-s23-side")

        _log({"seq": seq, "path": self.path, "method": "POST", "kind": kind,
              "headers": redact_headers(self.headers),
              "tool_names": names, "body": body})
        sys.stderr.write("s23 %s #%d %s tools=%d\n" % (self.path, seq, kind, len(names)))
        sys.stderr.flush()
        try:
            self._sse(payload)
        except (BrokenPipeError, ConnectionResetError, OSError) as e:
            sys.stderr.write("s23: response aborted by client: %r\n" % (e,))
            sys.stderr.flush()


class Server(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    srv = Server(("127.0.0.1", PORT), Handler)
    sys.stderr.write("s23 canned provider on 127.0.0.1:%d\n" % PORT)
    sys.stderr.flush()
    srv.serve_forever()
