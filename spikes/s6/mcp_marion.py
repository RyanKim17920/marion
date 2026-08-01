#!/usr/bin/env python3
"""Minimal stdio MCP server exposing marion's `report` tool. Logs every frame.

S6 uses this to answer: does `codex exec` host MCP servers, and does the child
actually reach `report`?  Log path comes from MARION_MCP_LOG.
"""
import sys
import json
import os

LOG = open(os.getenv("MARION_MCP_LOG", "/tmp/marion-mcp.log"), "a", buffering=1)


def log(direction, obj):
    LOG.write(json.dumps({"dir": direction, "frame": obj}) + "\n")


def send(obj):
    log("out", obj)
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


TOOLS = [{
    "name": "report",
    "description": "Return the task result to marion. Call exactly once when done.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "narrative": {"type": "string", "description": "what you did"},
            "result_commits": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["narrative"],
    },
}]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue
    log("in", req)
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "marion", "version": "0.0.1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": rid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        args = req.get("params", {}).get("arguments", {})
        LOG.write(json.dumps({"dir": "REPORT_RECEIVED", "args": args}) + "\n")
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "content": [{"type": "text", "text": "contract recorded"}],
            "isError": False}})
    elif method and method.startswith("notifications/"):
        pass
    elif rid is not None:
        send({"jsonrpc": "2.0", "id": rid,
              "error": {"code": -32601, "message": "no method " + str(method)}})
