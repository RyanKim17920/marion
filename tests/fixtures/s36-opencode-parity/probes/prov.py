# OpenAI-compatible SSE provider: text turn with usage; logs each request with a timestamp and tool names.
import json, sys, time, http.server
LOG = sys.argv[2]; CALLTOOL = sys.argv[4] if len(sys.argv) > 4 else None; HOLD = float(sys.argv[3]) if len(sys.argv) > 3 else 0
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        n = int(self.headers.get("content-length", 0)); body = json.loads(self.rfile.read(n) or b"{}")
        tools = [t.get("function", {}).get("name") for t in body.get("tools", [])]
        open(LOG, "a").write(json.dumps({"t": time.time(), "path": self.path, "tools": tools, "body": body}) + "\n")
        time.sleep(HOLD)
        self.send_response(200); self.send_header("content-type", "text/event-stream"); self.end_headers()
        def ch(d): self.wfile.write(("data: " + json.dumps(d) + "\n\n").encode()); self.wfile.flush()
        b = {"id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "m"}
        last = (body.get("messages") or [{}])[-1]
        if CALLTOOL and CALLTOOL in tools and last.get("role") != "tool":
            ch({**b, "choices": [{"index": 0, "delta": {"role": "assistant", "content": None, "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": CALLTOOL, "arguments": "{}"}}]}, "finish_reason": None}]})
            ch({**b, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
        else:
            ch({**b, "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": None}]})
            ch({**b, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        ch({**b, "choices": [], "usage": {"prompt_tokens": 1000, "completion_tokens": 50, "total_tokens": 1050, "prompt_tokens_details": {"cached_tokens": 300}, "completion_tokens_details": {"reasoning_tokens": 7}}})
        self.wfile.write(b"data: [DONE]\n\n"); self.wfile.flush()
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
