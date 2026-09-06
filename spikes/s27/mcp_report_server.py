#!/usr/bin/env python3
"""A minimal MCP stdio server that offers exactly marion's `report` verb and logs every frame.

Copied from spikes/s21/mcp_echo_server.py (same tool, same transcript format) with one addition:
`S27_MCP_ISERROR=1` makes `tools/call` answer `isError: true` with a refusal text, so the harness's
error-result frame can be captured.

Usage: mcp_report_server.py <logfile>
"""
import json
import os
import sys

LOG = open(sys.argv[1], "a", buffering=1)
ISERROR = os.getenv("S27_MCP_ISERROR") == "1"


def log(direction, obj):
    LOG.write(json.dumps({"dir": direction, "frame": obj}) + "\n")


def send(obj):
    log("out", obj)
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


TOOLS = [
    {
        "name": "report",
        "description": "Report the outcome of this task back to marion.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "narrative": {"type": "string"},
                "result_commits": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["narrative"],
        },
    }
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except json.JSONDecodeError:
        log("bad", line)
        continue
    log("in", req)
    method, rid = req.get("method"), req.get("id")
    if rid is None:
        continue  # a notification; nothing to answer
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": req.get("params", {}).get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "marion", "version": "0.0.0"},
        }})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": rid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        if ISERROR:
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": "refused: not authorized"}],
                "isError": True,
            }})
        else:
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": "recorded"}],
                "isError": False,
            }})
    else:
        send({"jsonrpc": "2.0", "id": rid,
              "error": {"code": -32601, "message": f"no method {method}"}})
