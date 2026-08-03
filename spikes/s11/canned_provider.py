#!/usr/bin/env python3
"""Canned Anthropic-Messages provider for spike S11 (S1's interrupt protocol over a pty).

Zero cost: no model is called and no API key is used. `ANTHROPIC_BASE_URL` points here.

Turn dispatch is on request *shape*, not position — design round 16: Claude Code 2.1.220
issues a session-title request to the same base URL concurrently with the first real turn,
so a positional responder hands the scripted turn to the wrong caller. A request whose
`tools` array is non-empty is a real turn; anything else gets a fixed title stub.

Scripted real turns:
  turn 1 — a *slow* text stream (one number per line, S11_TICK_MS apart, S11_TICKS of them).
           This is the turn the harness interrupts mid-stream, and it must stay open long
           enough to be interrupted. The Rust CannedServer answers with a single
           Content-Length body, which cannot be interrupted mid-stream; this one streams
           with chunked transfer-encoding and sleeps between frames.
  turn 2+ — the follow-up: one short text turn, "OK-AFTER-INTERRUPT" (S1's marker string).

Nothing about the request is logged except its path, method and a tool count. No header
value is ever recorded.
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn

PORT = int(os.getenv("S11_PORT", "8111"))
TICK_MS = int(os.getenv("S11_TICK_MS", "25"))
TICKS = int(os.getenv("S11_TICKS", "300"))
REQLOG = os.getenv("S11_REQLOG", "")

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

    # -- chunked SSE -------------------------------------------------------
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
            self._chunk(msg_start("msg_s11_title") + text_block_start()
                        + text_delta('{"title":"marion s11"}') + turn_end())
            self._end_stream()
            return

        with _lock:
            _turn["n"] += 1
            turn = _turn["n"]
        _log({"path": self.path, "kind": "turn", "turn": turn, "tools": len(tools)})

        if turn == 1:
            self._slow_turn()
        else:
            self._begin_stream()
            self._chunk(msg_start("msg_s11_after") + text_block_start()
                        + text_delta("OK-AFTER-INTERRUPT") + turn_end())
            self._end_stream()

    def _slow_turn(self):
        """One number per line, TICK_MS apart. Interruptible by construction."""
        self._begin_stream()
        try:
            self._chunk(msg_start("msg_s11_long") + text_block_start())
            for i in range(1, TICKS + 1):
                time.sleep(TICK_MS / 1000.0)
                self._chunk(text_delta("%d\n" % i))
            self._chunk(turn_end())
            self._end_stream()
        except (BrokenPipeError, ConnectionResetError, OSError) as e:
            sys.stderr.write("s11: slow turn aborted by client: %r\n" % (e,))
            sys.stderr.flush()


class Server(ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    srv = Server(("127.0.0.1", PORT), Handler)
    sys.stderr.write("s11 canned provider on 127.0.0.1:%d tick=%dms ticks=%d\n"
                     % (PORT, TICK_MS, TICKS))
    sys.stderr.flush()
    srv.serve_forever()
