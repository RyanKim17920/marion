# A stdio MCP server that holds its initialize answer for DELAY seconds, logging timestamps.
import json, sys, time
from os import getenv
DELAY = float(getenv("SLOW_DELAY", "8")); CALL = float(getenv("SLOW_CALL", "0")); LOG = getenv("SLOW_LOG")
def log(ev): open(LOG, "a").write(json.dumps({"t": time.time(), **ev}) + "\n")
log({"ev": "start"})
for line in sys.stdin:
    m = json.loads(line); meth = m.get("method")
    log({"ev": "recv", "method": meth})
    if "id" not in m: continue
    if meth == "initialize":
        time.sleep(DELAY)
        r = {"protocolVersion": m["params"].get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "slow", "version": "0"}}
    elif meth == "tools/list":
        r = {"tools": [{"name": "report", "description": "report", "inputSchema": {"type": "object", "properties": {}}}]}
    elif meth == "tools/call":
        time.sleep(CALL)
        r = {"content": [{"type": "text", "text": "ok"}]}
    else:
        r = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}) + "\n"); sys.stdout.flush()
    log({"ev": "sent", "method": meth})
