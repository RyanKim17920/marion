#!/usr/bin/env python3
"""A minimal MCP stdio server offering exactly marion's `report` verb, logging every frame.

Adapted from spikes/s21/mcp_echo_server.py for S26 (`goose run`). Differences:
  * the log path comes from `S26_MCP_LOG` (goose's `--with-extension` passes env
    assignments before the command, so the env is how the harness hands it in;
    argv[1] still works as a fallback).
  * `S26_MCP_ISERROR=1` makes every `tools/call` answer `isError: true`, so the
    harness's failed-tool frame and exit code can be measured.
  * `S26_MCP_ARGV` is *recorded* at startup so the transcript proves how goose
    parsed `[name:]ENV=v cmd args...`.

Not a marion component and not shipped: `marion-supervisor mcp` is the real bridge.
"""
import json
import os
import sys

LOG_PATH = os.getenv("S26_MCP_LOG") or (sys.argv[1] if len(sys.argv) > 1 else "/dev/null")
LOG = open(LOG_PATH, "a", buffering=1)
ISERROR = os.getenv("S26_MCP_ISERROR") == "1"


def log(direction, obj):
    LOG.write(json.dumps({"dir": direction, "frame": obj}) + "\n")


def send(obj):
    log("out", obj)
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


log("meta", {"argv": sys.argv, "cwd": os.getcwd(),
             "env_seen": {k: v for k, v in os.environ.items() if k.startswith("S26_")}})

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
    elif method in ("ping",):
        send({"jsonrpc": "2.0", "id": rid, "result": {}})
    else:
        send({"jsonrpc": "2.0", "id": rid,
              "error": {"code": -32601, "message": f"no method {method}"}})
