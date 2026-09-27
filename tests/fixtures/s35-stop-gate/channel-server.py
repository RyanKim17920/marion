#!/usr/bin/env python3
"""Minimal stdio MCP server that declares the claude/channel capability and exposes no tools.

Only its presence matters: it lets the session be launched with
`--dangerously-load-development-channels server:<key>`, the shape marion's native claude uses.
"""
import json
import sys

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "initialize":
        result = {
            "protocolVersion": msg.get("params", {}).get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}, "experimental": {"claude/channel": {}}},
            "serverInfo": {"name": "s35probe", "version": "0"},
        }
    elif method == "tools/list":
        result = {"tools": []}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
