#!/usr/bin/env python3
"""Canned **OpenAI-compatible** provider for spike S25 (Qwen Code headless at $0.00).

Adapted from `spikes/s23/canned_provider.py` (opencode acp). Same wire: `POST {baseURL}/chat/completions`,
SSE `chat.completion.chunk` deltas. Zero cost: no model is called; the key handed to qwen is the
literal `marion-canned-credential-25aa`, minted in `run.py`, and every secret-looking header value
is logged as a length only.

Turn dispatch is on request *shape*, never on a counter (S11 round 16: harnesses issue side
requests concurrently with the real turn). The script the canned model follows, reading each
request's own `messages[]` and `tools[]`:

  * `tools[]` offers `S25_WRITE_TOOL` (default `write_file`) and no assistant turn has called it
    yet   -> a streamed `tool_calls` delta naming the write tool (path/content), so the fixture
    exercises a built-in tool *and* the MCP tool in one run, as s24 did.
  * `tools[]` offers `S25_TOOL` (default `report`) and no assistant turn has called it yet
    -> a streamed `tool_calls` delta naming it. The provider **looks the name up in the array the
    harness sent** and never emits a name that is not there (s14: an unseen name is silently
    ignored) -- so if qwen prefixes the MCP tool, `S25_TOOL` must be set to the prefixed spelling
    and the log's `tool_names` is where to read it.
  * otherwise -> one short text, `finish_reason: "stop"`.

`S25_STATUS=500` makes every POST answer `500 {"error": ...}` instead, for the retry measurement.
Every request body is logged verbatim except secret header values.
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn

PORT = int(os.getenv("S25_PORT", "8125"))
REQLOG = os.getenv("S25_REQLOG", "")
TOOL = os.getenv("S25_TOOL", "report")
WRITE_TOOL = os.getenv("S25_WRITE_TOOL", "write_file")
WRITE_PATH = os.getenv("S25_WRITE_PATH", "")
NARRATIVE = os.getenv("S25_NARRATIVE", "hello from qwen under a canned provider")
MODEL = os.getenv("S25_MODEL", "canned-1")
STATUS = int(os.getenv("S25_STATUS", "200"))

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


def chunk(delta, finish=None, cid="chatcmpl-s25"):
    return "data: %s\n\n" % json.dumps({
        "id": cid,
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })


def text_turn(text, cid="chatcmpl-s25-text"):
    return "".join([
        chunk({"role": "assistant", "content": ""}, cid=cid),
        chunk({"content": text}, cid=cid),
        chunk({}, finish="stop", cid=cid),
        "data: [DONE]\n\n",
    ])


def tool_call_turn(name, args, call_id, cid):
    """Streamed the way a real provider streams it: name first with empty arguments, JSON later."""
    return "".join([
        chunk({"role": "assistant", "content": None}, cid=cid),
        chunk({"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                               "function": {"name": name, "arguments": ""}}]}, cid=cid),
        chunk({"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                               "function": {"arguments": json.dumps(args)}}]}, cid=cid),
        chunk({}, finish="tool_calls", cid=cid),
        "data: [DONE]\n\n",
    ])


TOOL_SEARCH = "tool_search"
MAIN_SYSTEM_PREFIX = os.getenv("S25_MAIN_PREFIX", "You are Qwen Code")


def is_main_turn(body):
    """The real turn's system prompt opens `You are Qwen Code, a non-interactive CLI agent…`;
    side turns open differently (memory extraction: `You are now acting as the managed memory
    extraction subagent…`). Read off `provider.jsonl`, not guessed."""
    for m in body.get("messages", []) or []:
        if m.get("role") == "system":
            c = m.get("content")
            s = c if isinstance(c, str) else json.dumps(c)
            return MAIN_SYSTEM_PREFIX in (s or "")[:200]
    return True


def called_so_far(body):
    out = []
    for m in body.get("messages", []) or []:
        if m.get("role") != "assistant":
            continue
        for tc in m.get("tool_calls") or []:
            n = (tc.get("function") or {}).get("name")
            if n:
                out.append(n)
    return out


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

    def _raw(self, enc, status, ctype):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(enc)))
        self.end_headers()
        self.wfile.write(enc)
        self.wfile.flush()

    def _sse(self, payload):
        self._raw(payload.encode(), 200, "text/event-stream")

    def _json(self, obj, status=200):
        self._raw(json.dumps(obj).encode(), status, "application/json")

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
        done = called_so_far(body)

        if STATUS != 200:
            kind, payload = "error-%d" % STATUS, None
        elif not is_main_turn(body):
            # qwen's hidden side turns (measured: a "managed memory extraction subagent" prompt
            # that is *offered write_file*). Never hand a scripted tool call to one of those.
            kind, payload = "side-stub", text_turn("marion s25 side", "chatcmpl-s25-side")
        elif TOOL not in names and TOOL_SEARCH in names and TOOL_SEARCH not in done:
            # qwen 0.23.0 defers MCP tools behind `tool_search` unless told otherwise (README);
            # the canned model does what the reminder tells a real one to do.
            kind = "turn-call-tool-search"
            payload = tool_call_turn(TOOL_SEARCH, {"query": "select:%s" % TOOL},
                                     "call_s25_search_1", "chatcmpl-s25-search")
        elif WRITE_TOOL in names and WRITE_TOOL not in done:
            kind = "turn-call-write"
            payload = tool_call_turn(
                WRITE_TOOL,
                {"file_path": WRITE_PATH or os.path.join(os.getcwd(), "s25-note.txt"),
                 "content": "written by the canned model\n"},
                "call_s25_write_1", "chatcmpl-s25-write")
        elif TOOL in names and TOOL not in done:
            kind = "turn-call-report"
            payload = tool_call_turn(TOOL, {"narrative": NARRATIVE},
                                     "call_s25_report_1", "chatcmpl-s25-report")
        elif names and TOOL not in names:
            kind, payload = "turn-no-marion-tool", text_turn("S25-NO-MARION-TOOL")
        else:
            kind, payload = "turn-text", text_turn("S25-OK", "chatcmpl-s25-after")

        _log({"seq": seq, "t": round(time.time(), 3), "path": self.path, "method": "POST", "kind": kind,
              "headers": redact_headers(self.headers),
              "tool_names": names, "called_so_far": done, "body": body})
        sys.stderr.write("s25 %s #%d %s tools=%d\n" % (self.path, seq, kind, len(names)))
        sys.stderr.flush()
        try:
            if payload is None:
                self._json({"error": {"message": "canned failure", "type": "server_error"}}, STATUS)
            else:
                self._sse(payload)
        except (BrokenPipeError, ConnectionResetError, OSError) as e:
            sys.stderr.write("s25: response aborted by client: %r\n" % (e,))
            sys.stderr.flush()


class Server(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    srv = Server(("127.0.0.1", PORT), Handler)
    sys.stderr.write("s25 canned provider on 127.0.0.1:%d\n" % PORT)
    sys.stderr.flush()
    srv.serve_forever()
