#!/usr/bin/env python3
"""S21 — drive a real ACP agent all the way to a marion tool call.

`initialize` -> `session/new` (declaring marion's MCP server) -> `session/prompt`. Every frame in
both directions is recorded verbatim, plus the MCP server's own transcript, because the two
questions this answers are only answerable from the pair:

  1. Does ACP's `session/new` actually carry an MCP declaration to the agent — i.e. is there
     something behind a fifth `McpRoute` variant, or nothing?
  2. **What name does the model type?** s14: an unknown tool name is silently ignored by claude,
     gemini and opencode alike, so a guessed spelling produces a healthy-looking run with no tool.

Usage: acp_child_probe.py <label> [--prompt TEXT] -- <argv...>
"""
import json
import os
import subprocess
import sys
import threading
import time

args = sys.argv[1:]
label = args[0]
prompt = "Call the marion report tool with narrative \"hello from acp\". Do nothing else."
if "--prompt" in args:
    prompt = args[args.index("--prompt") + 1]
agent_argv = args[args.index("--") + 1:]

cwd = os.environ.get("PROBE_CWD", "/tmp/acpchild")
os.makedirs(cwd, exist_ok=True)
mcp_log = os.path.join(cwd, f"{label}-mcp.jsonl")
open(mcp_log, "w").close()
echo = os.path.join(os.path.dirname(os.path.abspath(__file__)), "mcp_echo_server.py")

p = subprocess.Popen(agent_argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.PIPE, cwd=cwd, text=True, bufsize=1)
frames, errs = [], []


def pump(f, sink):
    for line in f:
        sink.append(line.rstrip("\n"))


for f, sink in ((p.stdout, frames), (p.stderr, errs)):
    threading.Thread(target=pump, args=(f, sink), daemon=True).start()


def send(obj):
    p.stdin.write(json.dumps(obj) + "\n")
    p.stdin.flush()


def answer_agent_requests(upto):
    """The agent calls *us* mid-turn (permissions, fs). Answer permissively; refusing would
    measure the probe's policy instead of the agent's vocabulary."""
    for raw in frames[:upto]:
        try:
            m = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if "method" not in m or "id" not in m or m["id"] in answered:
            continue
        answered.add(m["id"])
        meth = m["method"]
        if meth == "session/request_permission":
            opts = m.get("params", {}).get("options", [])
            pick = next((o["optionId"] for o in opts if "allow" in o.get("kind", "")), None)
            pick = pick or (opts[0]["optionId"] if opts else "allow")
            send({"jsonrpc": "2.0", "id": m["id"],
                  "result": {"outcome": {"outcome": "selected", "optionId": pick}}})
        elif meth == "fs/read_text_file":
            try:
                with open(m["params"]["path"]) as fh:
                    send({"jsonrpc": "2.0", "id": m["id"], "result": {"content": fh.read()}})
            except OSError as e:
                send({"jsonrpc": "2.0", "id": m["id"],
                      "error": {"code": -32000, "message": str(e)}})
        elif meth == "fs/write_text_file":
            send({"jsonrpc": "2.0", "id": m["id"], "result": {}})
        else:
            send({"jsonrpc": "2.0", "id": m["id"],
                  "error": {"code": -32601, "message": f"probe has no {meth}"}})


answered = set()


def wait_for_id(want, secs):
    end = time.time() + secs
    while time.time() < end:
        n = len(frames)
        answer_agent_requests(n)
        for raw in frames[:n]:
            try:
                m = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if m.get("id") == want and ("result" in m or "error" in m):
                return m
        if p.poll() is not None:
            time.sleep(0.5)
            answer_agent_requests(len(frames))
            break
        time.sleep(0.2)
    return None


send({"jsonrpc": "2.0", "id": 0, "method": "initialize",
      "params": {"protocolVersion": 1,
                 "clientCapabilities": {"fs": {"readTextFile": True, "writeTextFile": True},
                                        "terminal": True}}})
init = wait_for_id(0, 30)

session = None
if init and "result" in init:
    send({"jsonrpc": "2.0", "id": 1, "method": "session/new",
          "params": {"cwd": cwd,
                     "mcpServers": [{"name": "marion", "command": sys.executable,
                                     "args": [echo, mcp_log], "env": []}]}})
    newsess = wait_for_id(1, 60)
    if newsess and "result" in newsess:
        session = newsess["result"].get("sessionId")

promptresp = None
if session:
    send({"jsonrpc": "2.0", "id": 2, "method": "session/prompt",
          "params": {"sessionId": session, "prompt": [{"type": "text", "text": prompt}]}})
    promptresp = wait_for_id(2, 180)

try:
    with open(mcp_log) as fh:
        mcp = [json.loads(l) for l in fh if l.strip()]
except OSError:
    mcp = []

print(json.dumps({"label": label, "argv": agent_argv, "cwd": cwd,
                  "session": session, "prompt_response": promptresp,
                  "exit": p.poll(), "frames": frames, "stderr": errs[:40],
                  "mcp": mcp}, indent=2))
try:
    p.kill()
    p.wait(timeout=5)
except Exception:
    pass
