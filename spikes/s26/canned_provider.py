#!/usr/bin/env python3
"""Canned **OpenAI-compatible** provider for spike S26 (`goose run` at $0.00).

Adapted from spikes/s23/canned_provider.py (env prefix S26_). goose's `openai`
provider speaks `POST {OPENAI_HOST}/{OPENAI_BASE_PATH}` with SSE
`chat.completion.chunk` deltas, so the S23 stub fits with two additions:

  * `S26_FAIL500=1` -> every POST answers HTTP 500 with a JSON error body, so
    the harness's retry / exit behaviour on a provider fault can be measured.
  * the default `S26_TOOL` is `marion__report`, goose's `<extension>__<tool>`
    spelling. The provider still only emits the call if that exact name is in
    the `tools[]` array goose sent; a wrong guess shows up in the request log as
    `turn-no-marion-tool`, never as a false pass.

Zero cost: no model is called and no real credential exists -- the key in
`OPENAI_API_KEY` is the literal string `marion-canned-credential-26bb`, minted
in run.py, and it is redacted out of the request log (header values are never
recorded, only their lengths).

Turn dispatch is on request *shape*, never on a counter (S11 round 16: harnesses
issue side requests concurrently with the real turn):

  * a request carrying a `role: "tool"` message  -> the turn **after** the MCP
    call came back -> one short text, `finish_reason: "stop"`.
  * a request whose `tools[]` contains `S26_TOOL` -> the **real** turn ->
    a streamed `tool_calls` delta naming that tool.
  * tools present but not ours -> `turn-no-marion-tool`, a text reply.
  * anything else (no tools) -> a title/summary stub.

Every request body is logged verbatim except secret header values.
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn

PORT = int(os.getenv("S26_PORT", "8126"))
REQLOG = os.getenv("S26_REQLOG", "")
TOOL = os.getenv("S26_TOOL", "marion__report")
NARRATIVE = os.getenv("S26_NARRATIVE", "hello from goose under a canned provider")
MODEL = os.getenv("S26_MODEL", "canned-1")
FAIL500 = os.getenv("S26_FAIL500") == "1"
MODELS_PLAIN = os.getenv("S26_MODELS_PLAIN") == "1"

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


def chunk(delta, finish=None, cid="chatcmpl-s26"):
    return "data: %s\n\n" % json.dumps({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })


def usage_chunk(cid):
    return "data: %s\n\n" % json.dumps({
        "id": cid, "object": "chat.completion.chunk", "created": int(time.time()),
        "model": MODEL, "choices": [],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
    })


def text_turn(text, cid="chatcmpl-s26-text"):
    return "".join([
        chunk({"role": "assistant", "content": ""}, cid=cid),
        chunk({"content": text}, cid=cid),
        chunk({}, finish="stop", cid=cid),
        usage_chunk(cid),
        "data: [DONE]\n\n",
    ])


def tool_call_turn(name, args, call_id="call_s26_marion_1", cid="chatcmpl-s26-tool"):
    """Streamed the way a real provider streams it: the name arrives on the
    opening delta with empty arguments, the JSON arrives as a later fragment."""
    return "".join([
        chunk({"role": "assistant", "content": None}, cid=cid),
        chunk({"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                               "function": {"name": name, "arguments": ""}}]}, cid=cid),
        chunk({"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                               "function": {"arguments": json.dumps(args)}}]}, cid=cid),
        chunk({}, finish="tool_calls", cid=cid),
        usage_chunk(cid),
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
        # `/v1/models` if goose ever probes. A hit here is itself a finding.
        _log({"t": round(time.time(), 3), "path": self.path, "method": "GET", "kind": "get",
              "headers": redact_headers(self.headers)})
        sys.stderr.write("s26 GET %s\n" % self.path)
        if MODELS_PLAIN:
            # S26_MODELS_PLAIN=1: a 200 that is not JSON. Does goose care?
            enc = b"marion canned provider\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(enc)))
            self.end_headers()
            self.wfile.write(enc)
        elif self.path.rstrip("/").endswith("models"):
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

        if FAIL500:
            kind, payload = "fail-500", None
        elif has_tool_role(body):
            kind, payload = "turn-after-tool", text_turn("S26-OK", "chatcmpl-s26-after")
        elif TOOL in names:
            kind = "turn-with-marion-tool"
            payload = tool_call_turn(TOOL, {"narrative": NARRATIVE})
        elif names:
            kind = "turn-no-marion-tool"
            payload = text_turn("S26-NO-MARION-TOOL")
        else:
            kind, payload = "side-stub", text_turn("marion s26", "chatcmpl-s26-side")

        _log({"t": round(time.time(), 3), "seq": seq, "path": self.path, "method": "POST", "kind": kind,
              "headers": redact_headers(self.headers),
              "tool_names": names, "body": body})
        sys.stderr.write("s26 %s #%d %s tools=%d\n" % (self.path, seq, kind, len(names)))
        sys.stderr.flush()
        try:
            if payload is None:
                self._json({"error": {"message": "canned 500 from s26",
                                      "type": "server_error"}}, 500)
                return
            self._sse(payload)
        except (BrokenPipeError, ConnectionResetError, OSError) as e:
            sys.stderr.write("s26: response aborted by client: %r\n" % (e,))
            sys.stderr.flush()


class Server(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    srv = Server(("127.0.0.1", PORT), Handler)
    sys.stderr.write("s26 canned provider on 127.0.0.1:%d\n" % PORT)
    sys.stderr.flush()
    srv.serve_forever()
