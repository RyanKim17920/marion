"""S34/pi: drive pi's headless JSON surface against a canned provider and a stub MCP server.

    python3 spikes/s34/pi_run.py <out-dir>

Every scenario gets its own run directory with a relocated PI_CODING_AGENT_DIR holding a
models.json that names the local provider. marion's own extension template
(crates/marion-harness/src/pi_extension.js) is rendered around the stub MCP server, so what is
measured is the declaration marion ships. No vendor endpoint, no login, no real key.
"""
import json, os, re, shutil, socket, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
TEMPLATE = os.path.join(REPO, "crates/marion-harness/src/pi_extension.js")
KEY = "marion-canned-credential-34pi"
OUT = os.path.abspath(sys.argv[1])
os.makedirs(OUT, exist_ok=True)


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def run(name, prompt, provider_env=None, mcp_mode=None, extra_args=(), session=None, with_ext=True, run_dir=None):
    d = run_dir or tempfile.mkdtemp(prefix=f"s34-{name}-")
    agent = os.path.join(d, "agent"); work = os.path.join(d, "work")
    os.makedirs(agent, exist_ok=True); os.makedirs(work, exist_ok=True)
    port = free_port()
    json.dump({"providers": {"marion": {"baseUrl": f"http://127.0.0.1:{port}/v1", "api": "openai-completions",
                                        "apiKey": KEY, "models": [{"id": "canned-1"}]}}},
              open(os.path.join(agent, "models.json"), "w"))
    reqlog = os.path.join(d, f"{name}.requests.jsonl"); mcplog = os.path.join(d, f"{name}.mcp.jsonl")
    penv = dict(os.environ, **(provider_env or {}))
    prov = subprocess.Popen([sys.executable, os.path.join(HERE, "canned_provider.py"), str(port), reqlog], env=penv,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(0.4)
    args = ["pi", "-p", "--mode", "json"]
    if session:
        args += ["--session", session]
    args += ["--no-extensions", "--provider", "marion", "--model", "canned-1"]
    if with_ext:
        server = {"command": sys.executable, "args": [os.path.join(HERE, "mcp_report_server.py"), mcplog] + ([mcp_mode] if mcp_mode else []),
                  "env": {"MARION_AGENT_ID": "019f-child"}}
        src = open(TEMPLATE).read().replace("__MARION_SERVER__", json.dumps(server)).replace("__MARION_PREFIX__", json.dumps("mcp__marion__"))
        ext = os.path.join(d, "marion-pi.js"); open(ext, "w").write(src)
        args += ["--tools", "read,write,edit,mcp__marion__report", "-e", ext]
    args += list(extra_args) + [prompt]
    env = dict(os.environ, PI_CODING_AGENT_DIR=agent, PI_OFFLINE="1")
    t0 = time.time()
    p = subprocess.run(args, cwd=work, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=180)
    wall = round(time.time() - t0, 1)
    prov.kill(); prov.wait()
    return d, p, wall, reqlog, mcplog, args


UUIDS = {}


def redact(s, d):
    s = s.replace(KEY, "<KEY>").replace(d, "<RUN-DIR>").replace(os.path.realpath(d), "<RUN-DIR>")
    s = s.replace(sys.executable, "<PYTHON>").replace(REPO, "<REPO>").replace(os.path.expanduser("~"), "<HOME>")
    s = re.sub(r"[^\s\"']*/s34-[a-z0-9]+-[A-Za-z0-9_]+", "<RUN-DIR>", s)
    s = re.sub(r"--[A-Za-z0-9-]*s34-[A-Za-z0-9_-]*--", "<RUN-DIR-SLUG>", s)

    def u(m):
        return UUIDS.setdefault(m.group(0), f"<UUID-{len(UUIDS) + 1}>")
    s = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", u, s)
    s = re.sub(r'"timestamp":\s?("[^"]*"|\d+)', '"timestamp":"<T>"', s)
    return s


def keep(name, d, p, wall, reqlog, mcplog, args, first_request=False):
    assert KEY not in p.stdout
    lines = [l for l in p.stdout.splitlines() if '"type":"message_update"' not in l]
    open(os.path.join(OUT, f"pi-{name}.stdout.jsonl"), "w").write(redact("\n".join(lines) + "\n", d))
    meta = {"argv": args, "exit": p.returncode, "wall_s": wall,
            "requests": sum(1 for _ in open(reqlog)) if os.path.exists(reqlog) else 0,
            "stderr": p.stderr.strip()[:2000]}
    open(os.path.join(OUT, f"pi-{name}.run.json"), "w").write(redact(json.dumps(meta, indent=1), d) + "\n")
    if os.path.exists(mcplog):
        shutil.copy(mcplog, os.path.join(OUT, f"pi-{name}.mcp.jsonl"))
    if first_request and os.path.exists(reqlog):
        r = json.loads(open(reqlog).readline())
        for m in r["body"].get("messages", []):
            if m.get("role") == "system":
                m["content"] = f"<system prompt, {len(m['content'])} chars>"
        r["headers"] = {k: (f"<{len(v)} chars>" if k.lower() == "authorization" else v) for k, v in r["headers"].items()}
        for t in r["body"].get("tools", []):
            if not t["function"]["name"].startswith("mcp__"):
                t["function"]["description"] = f"<{len(t['function'].get('description', ''))} chars>"
        open(os.path.join(OUT, f"pi-{name}.provider-request-1.json"), "w").write(redact(json.dumps(r, indent=1), d) + "\n")


REPORT = {"TOOL": "mcp__marion__report", "ARGS": json.dumps({"narrative": "hello from pi under a canned provider"}), "TEXT": "reported."}
keep("report", *run("report", "do the task", REPORT), first_request=True)
keep("report-iserror", *run("iserror", "do the task", REPORT, mcp_mode="err"))
keep("provider-500", *run("p500", "do the task", {"FAIL": "500"}))
keep("provider-401", *run("p401", "do the task", {"FAIL": "401"}))
keep("retry-then-report", *run("retry", "do the task", dict(REPORT, FAILN="1")))
keep("empty-tools", *run("notools", "do the task", {}, with_ext=False, extra_args=["--tools", ""]), first_request=True)
d, p, *rest = run("resume1", "first turn", {"TEXT": "turn one answer"})
keep("resume-turn-1", d, p, *rest)
sid = json.loads(p.stdout.splitlines()[0])["id"]
keep("resume-turn-2", *run("resume2", "second turn", {"TEXT": "turn two answer"}, session=sid, run_dir=d), first_request=True)
keep("resume-unknown", *run("resumex", "x", {}, session="0000dead-beef-0000-0000-000000000000"))
bad = tempfile.mkdtemp(prefix="s34-bad-")
open(os.path.join(bad, "marion-pi.js"), "w").write(open(TEMPLATE).read().replace("__MARION_SERVER__", json.dumps({"command": "/nonexistent/bridge", "args": [], "env": {}})).replace("__MARION_PREFIX__", '"mcp__marion__"'))
keep("bridge-missing", *run("bridge", "x", {}, with_ext=False, extra_args=["--tools", "mcp__marion__report", "-e", os.path.join(bad, "marion-pi.js")]))
print("wrote", OUT)
