# Minimal fake OpenAI Chat Completions + Anthropic Messages provider for measurement.
# usage: fakeoai.py PORT LOG ; vars TOOL=<name to call> ARGS=<json> TEXT=<final text> FAIL=500
import http.server, json, sys
from os import getenv
PORT = int(sys.argv[1]); LOG = sys.argv[2]
TOOL = getenv("TOOL", ""); ARGS = getenv("ARGS", '{"narrative":"hello from fake"}'); TEXT = getenv("TEXT", "done.")
FAIL = getenv("FAIL")
FAILN = [int(getenv("FAILN", "0"))]


def send(h, code, body, ctype="application/json"):
    b = body.encode() if isinstance(body, str) else body
    h.send_response(code); h.send_header("content-type", ctype); h.send_header("content-length", str(len(b))); h.end_headers(); h.wfile.write(b)


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        with open(LOG, "a") as f:
            f.write(json.dumps({"method": "GET", "path": self.path}) + "\n")
        send(self, 200, json.dumps({"object": "list", "data": [{"id": "canned-1", "object": "model"}]}))

    def do_POST(self):
        n = int(self.headers.get("content-length", 0)); raw = self.rfile.read(n)
        try:
            j = json.loads(raw)
        except Exception:
            j = {}
        with open(LOG, "a") as f:
            f.write(json.dumps({"path": self.path, "headers": {k: v for k, v in self.headers.items() if k.lower() in ("authorization", "x-api-key", "user-agent")}, "body": j}) + "\n")
        if FAILN[0] > 0:
            FAILN[0] -= 1
            return send(self, 500, json.dumps({"error": {"message": "canned transient", "type": "server_error"}}))
        if FAIL:
            return send(self, int(FAIL), json.dumps({"error": {"message": "canned failure", "type": "server_error"}}))
        msgs = j.get("messages", []) or j.get("input", []); tools = j.get("tools") or []
        names = [t.get("function", {}).get("name") or t.get("name") for t in tools]
        anth = "/messages" in self.path
        def is_result(m):
            if m.get("role") == "tool" or m.get("type") == "function_call_output":
                return True
            c = m.get("content")
            return isinstance(c, list) and any(isinstance(x, dict) and x.get("type") == "tool_result" for x in c)
        done_tool = any(is_result(m) for m in msgs)
        call = TOOL and TOOL in names and not done_tool
        stream = j.get("stream")
        if anth:
            if call:
                content = [{"type": "tool_use", "id": "toolu_1", "name": TOOL, "input": json.loads(ARGS)}]; stop = "tool_use"
            else:
                content = [{"type": "text", "text": TEXT}]; stop = "end_turn"
            msg = {"id": "msg_1", "type": "message", "role": "assistant", "model": j.get("model", "m"), "content": content, "stop_reason": stop, "stop_sequence": None, "usage": {"input_tokens": 11, "output_tokens": 7}}
            if not stream:
                return send(self, 200, json.dumps(msg))
            evs = [("message_start", {"type": "message_start", "message": {**msg, "content": [], "stop_reason": None}})]
            for i, c in enumerate(content):
                if c["type"] == "text":
                    evs += [("content_block_start", {"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": ""}}), ("content_block_delta", {"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": c["text"]}})]
                else:
                    evs += [("content_block_start", {"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": c["id"], "name": c["name"], "input": {}}}), ("content_block_delta", {"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": json.dumps(c["input"])}})]
                evs.append(("content_block_stop", {"type": "content_block_stop", "index": i}))
            evs += [("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 7}}), ("message_stop", {"type": "message_stop"})]
            return send(self, 200, "".join(f"event: {e}\ndata: {json.dumps(d)}\n\n" for e, d in evs), "text/event-stream")
        if call:
            delta = {"role": "assistant", "content": None, "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": TOOL, "arguments": ARGS}}]}; fin = "tool_calls"
        else:
            delta = {"role": "assistant", "content": TEXT}; fin = "stop"
        usage = {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        if not stream:
            m = {"role": "assistant", "content": delta.get("content"), **({"tool_calls": delta["tool_calls"]} if call else {})}
            return send(self, 200, json.dumps({"id": "cc1", "object": "chat.completion", "created": 0, "model": j.get("model", "m"), "choices": [{"index": 0, "message": m, "finish_reason": fin}], "usage": usage}))
        base = {"id": "cc1", "object": "chat.completion.chunk", "created": 0, "model": j.get("model", "m")}
        chunks = [{**base, "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                  {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": fin}]},
                  {**base, "choices": [], "usage": usage}]
        send(self, 200, "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n", "text/event-stream")


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
