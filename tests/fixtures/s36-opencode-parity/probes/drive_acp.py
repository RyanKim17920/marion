# Minimal ACP client: initialize, session/new (mcpServers=[slow]), session/prompt; answers permission asks allow_once.
import json, subprocess, sys, time, threading
from os import getenv
out = open(sys.argv[1], "w"); slow_env = json.loads(sys.argv[2]); cwd = sys.argv[3]
p = subprocess.Popen(["opencode", "acp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, cwd=cwd, text=True)
t0 = time.time(); pending = {}; lock = threading.Lock()
def send(m): out.write(json.dumps({"t": time.time()-t0, "dir": "out", "m": m}) + "\n"); out.flush(); p.stdin.write(json.dumps(m) + "\n"); p.stdin.flush()
def reader():
    for line in p.stdout:
        m = json.loads(line); out.write(json.dumps({"t": time.time()-t0, "dir": "in", "m": m}) + "\n"); out.flush()
        if m.get("method") == "session/request_permission":
            opt = next(o for o in m["params"]["options"] if o["kind"] == "allow_once")
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"outcome": {"outcome": "selected", "optionId": opt["optionId"]}}})
        elif "id" in m and m["id"] in pending:
            pending.pop(m["id"]).set_result(m)
threading.Thread(target=reader, daemon=True).start()
class F:
    def __init__(s): s.e = threading.Event(); s.v = None
    def set_result(s, v): s.v = v; s.e.set()
def call(i, meth, params, to=600):
    f = F(); pending[i] = f; send({"jsonrpc": "2.0", "id": i, "method": meth, "params": params}); f.e.wait(to); return f.v
call(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}}})
r = call(2, "session/new", {"cwd": cwd, "mcpServers": [{"name": "slow", "command": "python3", "args": [getenv("SLOWMCP")], "env": [{"name": k, "value": v} for k, v in slow_env.items()]}]})
sid = r["result"]["sessionId"]
call(3, "session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "hi"}]})
p.terminate()
