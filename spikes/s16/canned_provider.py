#!/usr/bin/env python3
"""Canned Anthropic-Messages provider for spike S16.

Zero cost: no model is called and no API key is used. `ANTHROPIC_BASE_URL`
points here. Adapted from `spikes/s11/canned_provider.py`, which is the house
pattern for a claude-driving spike; the differences are only in the script.

Turn dispatch is on request *shape*, not position (S11's round-16 note): Claude
Code 2.1.x issues a session-title request to the same base URL concurrently
with the first real turn, so a positional responder hands the scripted turn to
the wrong caller. A request whose `tools` array is non-empty is a real turn;
anything else gets a fixed title stub.

The one real turn is a text stream paced over ~S16_TURN_MS. It is paced rather
than instant because Claude Code connects `--mcp-config` servers
**asynchronously** (measured on 2.1.220; see `crates/marion-supervisor/src/
main.rs`'s ready-file comment). A turn that completes instantly can let the run
finish before the MCP server has been asked for `tools/list` at all -- which
would leave S16 measuring the teardown of a server that was never connected,
i.e. measuring nothing while looking like it measured something.

Nothing about the request is logged except its path, kind and a tool count. No
header value is ever recorded.
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn

PORT = int(os.getenv("S16_PORT", "8116"))
TURN_MS = int(os.getenv("S16_TURN_MS", "4000"))
TICKS = int(os.getenv("S16_TICKS", "16"))
REQLOG = os.getenv("S16_REQLOG", "")

_lock = threading.Lock()
_turn = {"n": 0}


def _log(rec):
    if not REQLOG:
        return
    with _lock:
        with open(REQLOG, "a") as fh:
            fh.write(json.dumps(rec) + "\n")


def sse(event, data):
    return ("event: %s\ndata: %s\n\n" % (event, json.dumps(data))).encode()


def msg_start(mid):
    return sse("message_start", {"type": "message_start", "message": {
        "id": mid, "type": "message", "role": "assistant", "model": "canned",
        "content": [], "stop_reason": None,
        "usage": {"input_tokens": 0, "output_tokens": 0}}})


def text_block_start():
    return sse("content_block_start", {"type": "content_block_start", "index": 0,
                                       "content_block": {"type": "text", "text": ""}})


def text_delta(text):
    return sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                                       "delta": {"type": "text_delta", "text": text}})


def turn_end(stop_reason="end_turn"):
    return (sse("content_block_stop", {"type": "content_block_stop", "index": 0})
            + sse("message_delta", {"type": "message_delta",
                                    "delta": {"stop_reason": stop_reason},
                                    "usage": {"output_tokens": 0}})
            + sse("message_stop", {"type": "message_stop"}))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _begin_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

    def _chunk(self, payload):
        self.wfile.write(b"%x\r\n" % len(payload) + payload + b"\r\n")
        self.wfile.flush()

    def _end_stream(self):
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _json(self, obj, status=200):
        payload = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        self._json({"ok": True})

    def do_POST(self):
        n = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            body = json.loads(raw)
        except Exception:
            body = {}

        if "count_tokens" in self.path:
            _log({"path": self.path, "kind": "count_tokens"})
            self._json({"input_tokens": 1})
            return

        tools = body.get("tools") or []
        if not tools:
            _log({"path": self.path, "kind": "title_stub", "tools": 0})
            self._begin_stream()
            self._chunk(msg_start("msg_s16_title") + text_block_start()
                        + text_delta('{"title":"marion s16"}') + turn_end())
            self._end_stream()
            return

        with _lock:
            _turn["n"] += 1
            turn = _turn["n"]
        _log({"path": self.path, "kind": "turn", "turn": turn, "tools": len(tools)})
        self._paced_turn()

    def _paced_turn(self):
        """One word at a time over TURN_MS, so the MCP connect has time to land."""
        self._begin_stream()
        try:
            self._chunk(msg_start("msg_s16") + text_block_start())
            for i in range(TICKS):
                time.sleep(TURN_MS / 1000.0 / TICKS)
                self._chunk(text_delta("S16-OK " if i == 0 else "%d " % i))
            self._chunk(turn_end())
            self._end_stream()
        except (BrokenPipeError, ConnectionResetError, OSError) as e:
            sys.stderr.write("s16: turn aborted by client: %r\n" % (e,))
            sys.stderr.flush()


class Server(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    srv = Server(("127.0.0.1", PORT), Handler)
    sys.stderr.write("s16 canned provider on 127.0.0.1:%d turn=%dms\n"
                     % (PORT, TURN_MS))
    sys.stderr.flush()
    srv.serve_forever()
