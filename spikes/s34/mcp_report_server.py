# A stdio MCP server with one tool, `report`; logs every frame it receives to argv[1].
import json, sys, time
LOG = sys.argv[1]
ERR = len(sys.argv) > 2 and sys.argv[2] == "err"
SLOW = len(sys.argv) > 2 and sys.argv[2] == "slow"
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    with open(LOG, "a") as f:
        f.write(line)
    m = json.loads(line)
    if "id" not in m:
        continue
    meth = m.get("method")
    if meth == "initialize":
        out({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": m["params"].get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "0"}}})
    elif meth == "tools/list":
        out({"jsonrpc": "2.0", "id": m["id"], "result": {"tools": [{"name": "report", "description": "Report back.", "inputSchema": {"type": "object", "properties": {"narrative": {"type": "string"}}, "required": ["narrative"]}}]}})
    elif meth == "tools/call":
        if SLOW:
            time.sleep(3)
        out({"jsonrpc": "2.0", "id": m["id"], "result": {"content": [{"type": "text", "text": "refused: not authorized" if ERR else "recorded"}], "isError": ERR}})
    else:
        out({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "no such method"}})
