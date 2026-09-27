"""S34/pi: pi's `--mode rpc` as a typed control channel — steer, prompt, and a second turn.

    python3 spikes/s34/pi_rpc.py <out-dir>

One pi process per scenario. Commands go to stdin as JSONL and events are read from stdout. The
MCP stub runs in `slow` mode, where tools/call answers after 3 s, so a command written during
the call arrives while a turn is running. Every model call goes to the canned provider.
"""
import json, os, re, socket, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
TEMPLATE = os.path.join(REPO, "crates/marion-harness/src/pi_extension.js")
OUT = os.path.abspath(sys.argv[1])


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def scenario(name, script):
    d = tempfile.mkdtemp(prefix=f"s34-rpc-{name}-")
    agent = os.path.join(d, "agent"); work = os.path.join(d, "work")
    os.makedirs(agent); os.makedirs(work)
    port = free_port()
    json.dump({"providers": {"marion": {"baseUrl": f"http://127.0.0.1:{port}/v1", "api": "openai-completions",
                                        "apiKey": "k", "models": [{"id": "canned-1"}]}}}, open(os.path.join(agent, "models.json"), "w"))
    reqlog = os.path.join(d, "requests.jsonl")
    env = dict(os.environ, TOOL="mcp__marion__report", ARGS=json.dumps({"narrative": "rpc report"}), TEXT="rpc done")
    prov = subprocess.Popen([sys.executable, os.path.join(HERE, "canned_provider.py"), str(port), reqlog], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    server = {"command": sys.executable, "args": [os.path.join(HERE, "mcp_report_server.py"), os.path.join(d, "mcp.jsonl"), "slow"], "env": {}}
    ext = os.path.join(d, "marion-pi.js")
    open(ext, "w").write(open(TEMPLATE).read().replace("__MARION_SERVER__", json.dumps(server)).replace("__MARION_PREFIX__", '"mcp__marion__"'))
    p = subprocess.Popen(["pi", "--mode", "rpc", "--no-extensions", "--provider", "marion", "--model", "canned-1",
                          "--tools", "read,mcp__marion__report", "-e", ext], cwd=work,
                         env=dict(os.environ, PI_CODING_AGENT_DIR=agent, PI_SKIP_VERSION_CHECK="1"),
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    events = []
    t0 = time.time()

    def reader():
        for line in p.stdout:
            events.append(line)
    threading.Thread(target=reader, daemon=True).start()

    def send(o):
        events.append(json.dumps({"_sent": o, "_t": round(time.time() - t0, 1)}) + "\n")
        p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()

    def wait_for(pred, bound=30):
        end = time.time() + bound
        while time.time() < end:
            for l in list(events):
                try:
                    if pred(json.loads(l)):
                        return
                except ValueError:
                    pass
            time.sleep(0.05)
        raise SystemExit(f"{name}: timed out waiting")
    script(send, wait_for)
    p.stdin.close()
    code = p.wait(timeout=30)
    prov.kill()
    kept = [l for l in events if '"type": "message_update"' not in l and '"type":"message_update"' not in l]
    text = "".join(kept).replace(d, "<RUN-DIR>")
    text = re.sub(r'"timestamp":\s?("[^"]*"|\d+)', '"timestamp":"<T>"', text)
    text = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<UUID>", text)
    open(os.path.join(OUT, f"pi-rpc-{name}.stdout.jsonl"), "w").write(text)
    reqs = [json.loads(l)["body"] for l in open(reqlog)]
    summary = {"exit": code, "requests": [[(m["role"], m["content"] if isinstance(m["content"], str) else
                                             " ".join(c.get("text", "") for c in (m["content"] or []) if isinstance(c, dict)))
                                            for m in r["messages"] if m["role"] != "system"] for r in reqs]}
    open(os.path.join(OUT, f"pi-rpc-{name}.requests.json"), "w").write(json.dumps(summary, indent=1) + "\n")


def is_(t):
    return lambda j: j.get("type") == t


def steer_mid_tool(send, wait_for):
    send({"id": "1", "type": "prompt", "message": "first ZEBRA"})
    wait_for(is_("tool_execution_start"))
    send({"id": "2", "type": "prompt", "message": "bare prompt while streaming GIRAFFE"})
    send({"id": "3", "type": "steer", "message": "steer OKAPI"})
    send({"id": "4", "type": "follow_up", "message": "follow-up LLAMA"})
    wait_for(lambda j: j.get("type") == "agent_end" and "LLAMA" in json.dumps(j))
    send({"id": "5", "type": "prompt", "message": "second turn IBEX"})
    wait_for(lambda j: j.get("type") == "agent_end" and "IBEX" in json.dumps(j))


def abort_mid_tool(send, wait_for):
    send({"id": "1", "type": "prompt", "message": "first ZEBRA"})
    wait_for(is_("tool_execution_start"))
    send({"id": "2", "type": "abort"})
    wait_for(is_("agent_end"))


scenario("steer-mid-tool", steer_mid_tool)
scenario("abort-mid-tool", abort_mid_tool)
print("wrote", OUT)
