#!/usr/bin/env python3
"""A minimal MCP stdio server that offers exactly marion's `report` verb and logs every frame.

It exists to answer one question the other adapters answered years-equivalent ago and ACP has
never been asked: **what does an ACP agent call marion's tool, in the name the model types?**
s14's finding is that a guessed tool name is silently ignored, so the mapping has to be read off
a live agent rather than copied from the opencode `run` adapter.

Not a marion component and not shipped: `marion-supervisor mcp` is the real bridge. This server
is deliberately dumb so that anything interesting in the transcript came from the agent.

Usage: mcp_echo_server.py <logfile>
"""
import json
import sys

LOG = open(sys.argv[1], "a", buffering=1)


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
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "content": [{"type": "text", "text": "recorded"}],
            "isError": False,
        }})
    else:
        send({"jsonrpc": "2.0", "id": rid,
              "error": {"code": -32601, "message": f"no method {method}"}})
