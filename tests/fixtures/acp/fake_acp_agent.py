#!/usr/bin/env python3
"""A minimal ACP agent marion has never heard of, for `tests/acp_child.rs`.

It speaks ACP wire v1 over stdio and does exactly what a real agent does with marion's bridge:
starts every MCP server declared in `session/new`, handshakes with it (`initialize`,
`notifications/initialized`, `tools/list`), and — on the first `session/prompt` — writes one file into
the session's cwd and calls the bridge's `report` tool for real, mirroring the call as ACP
`tool_call` / `tool_call_update` frames.

The mirrored tool title is spelled `marion/report`: a **fifth** spelling, on purpose, one that no row
in `acp::AGENTS` was measured to use. The test this serves asserts that marion reads the report out of
a transcript in a spelling it has never seen, from an agent it has never named — the baseline "any
ACP agent works" path — so the spelling here must stay one the refinement table does not carry.

Runs nothing but the servers it is handed; no model, no network, no credential.
"""
import json
import os
import subprocess
import sys
import threading

CHILD_FILE = "src/marion_acp.txt"
CHILD_FILE_CONTENT = "marion: written by the fake ACP agent\n"
NARRATIVE = "Edited the worktree over ACP from an agent marion had never heard of."
TOOL_TITLE = "marion/report"

out_lock = threading.Lock()


def send(obj):
    with out_lock:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()


class McpServer:
    """One stdio MCP server the client declared, driven synchronously."""

    def __init__(self, decl):
        env = dict(os.environ)
        for pair in decl.get("env", []):
            env[pair["name"]] = pair["value"]
        self.name = decl["name"]
        self.proc = subprocess.Popen(
            [decl["command"], *decl.get("args", [])],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            text=True,
            bufsize=1,
        )
        self.next_id = 0
        self.tools = []

    def call(self, method, params):
        rid = self.next_id
        self.next_id += 1
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f"mcp server `{self.name}` closed its stdout")
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                continue
            if frame.get("id") == rid:
                return frame

    def notify(self, method, params=None):
        frame = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            frame["params"] = params
        self.proc.stdin.write(json.dumps(frame) + "\n")
        self.proc.stdin.flush()

    def handshake(self):
        self.call(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "fake-acp-agent", "version": "0.1.0"},
            },
        )
        self.notify("notifications/initialized")
        listed = self.call("tools/list", {})
        self.tools = [t["name"] for t in listed.get("result", {}).get("tools", [])]

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


servers = []
session = {"id": None, "cwd": None}


def update(session_id, body):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": body}})


def handle(frame):
    method = frame.get("method")
    rid = frame.get("id")
    params = frame.get("params") or {}
    if method == "initialize":
        send(
            {
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "protocolVersion": 1,
                    "agentInfo": {"name": "fake-acp-agent", "version": "0.1.0"},
                    "agentCapabilities": {"loadSession": False, "promptCapabilities": {}},
                    "authMethods": [],
                },
            }
        )
    elif method == "session/new":
        session["cwd"] = params.get("cwd") or os.getcwd()
        for decl in params.get("mcpServers", []):
            server = McpServer(decl)
            server.handshake()
            servers.append(server)
        session["id"] = "fake-session-1"
        send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": session["id"]}})
    elif method == "session/prompt":
        sid = params.get("sessionId") or session["id"]
        path = os.path.join(session["cwd"], CHILD_FILE)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as fh:
            fh.write(CHILD_FILE_CONTENT)
        marion = next((s for s in servers if s.name == "marion" and "report" in s.tools), None)
        if marion is None:
            send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
            return
        args = {"narrative": NARRATIVE}
        update(sid, {"sessionUpdate": "tool_call", "toolCallId": "call-1", "title": TOOL_TITLE,
                     "kind": "other", "status": "pending", "rawInput": {}})
        result = marion.call("tools/call", {"name": "report", "arguments": args})
        failed = "error" in result or result.get("result", {}).get("isError") is True
        text = json.dumps(result.get("result", result.get("error")))
        update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": "call-1",
                     "status": "failed" if failed else "completed", "rawInput": args,
                     "content": [{"type": "content", "content": {"type": "text", "text": text}}]})
        send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
    elif method == "session/cancel":
        pass
    elif rid is not None:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": f"fake-acp-agent does not implement `{method}`"}})


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            frame = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "method" in frame:
            handle(frame)
    for s in servers:
        s.close()


if __name__ == "__main__":
    main()
