#!/usr/bin/env python3
"""S25 — Qwen Code (`qwen -p … --output-format stream-json`) against a canned local provider, $0.00.

What this wires together, per scenario, in a fresh run directory:

  * `spikes/s25/canned_provider.py` on 127.0.0.1 — the model (OpenAI Chat Completions SSE).
  * `spikes/s25/mcp_report_server.py` — marion's stand-in bridge, offering exactly `report`,
    declared to qwen through the `mcpServers` map of `$QWEN_HOME/settings.json` (or a project
    `.qwen/settings.json`, scenario `project-settings`).
  * `qwen` itself, with `QWEN_HOME` pointed at the run dir so `~/.qwen` is never touched, the
    provider reached through `OPENAI_BASE_URL`/`OPENAI_API_KEY`/`OPENAI_MODEL`, and a hard
    `timeout` so no scenario can hang.

Every scenario writes: `stdout.jsonl`, `stderr.txt`, `exit.txt`, `argv.json`, `env.json`,
`provider.jsonl` (each request as the provider saw it), `mcp.jsonl` (the MCP transcript),
`home-after.txt` (what appeared under QWEN_HOME), and the settings document it used.

Usage: run.py <scenario>... [--out DIR]     (scenario `all` runs every one)
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PROVIDER = os.path.join(HERE, "canned_provider.py")
MCP = os.path.join(HERE, "mcp_report_server.py")
QWEN = shutil.which("qwen") or "/opt/homebrew/bin/qwen"

API_KEY = "marion-canned-credential-25aa"   # minted here; never a real credential
MODEL = "canned-1"
TIMEOUT = int(os.getenv("S25_TIMEOUT", "90"))

# The spelling qwen shows the model for the MCP tool. Measured off the baseline run's
# `provider.jsonl` `tool_names`; the provider only emits a name it can find there.
MCP_TOOL_NAME = os.getenv("S25_TOOL", "mcp__marion__report")

# Measured off the bundle (`chunk-4F7GQGXB.js`, Config.initialize): without this, MCP discovery
# runs *in the background* and tools from servers that connect after the first declaration list
# is built stay deferred behind `tool_search` for the whole session. With it, discovery blocks
# startup and the MCP tool is a first-class entry in `tools[]` from request one.
BLOCKING_MCP = {"QWEN_CODE_LEGACY_MCP_BLOCKING": "1"}

PROMPT = ("Write the file s25-note.txt if you have a tool for it, then call the report tool with a "
          "one-line narrative. Do nothing else.")


def say(msg):
    sys.stderr.write("[s25] %s\n" % msg)
    sys.stderr.flush()


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_port(port, secs=15):
    end = time.time() + secs
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), 1):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def run_bounded(argv, cwd, env, rundir):
    """Run qwen with stdout/stderr captured and a hard wall clock, killing the **process group**.

    Measured the hard way: `/opt/homebrew/bin/qwen` is `cli-entry.js`, a launcher that `spawnSync`s
    the real CLI as a child node process. Killing only the launcher PID (what `subprocess.run`'s
    timeout does) leaves the child running to completion — the first provider-500 run kept
    writing frames for 11 s after its 90 s timeout. So: new session, then `killpg`.
    """
    import signal
    with open(os.path.join(rundir, "stdout.jsonl"), "wb") as so, \
            open(os.path.join(rundir, "stderr.txt"), "wb") as se:
        p = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=so,
                             stderr=se, start_new_session=True)
        try:
            return p.wait(timeout=TIMEOUT)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            p.wait()
            return "timeout-%ds" % TIMEOUT


def snapshot(root):
    out = []
    for dp, dns, fns in os.walk(root):
        dns.sort()
        for f in sorted(fns):
            p = os.path.join(dp, f)
            out.append("%s\t%d" % (os.path.relpath(p, root), os.path.getsize(p)))
    return "\n".join(out) + "\n"


def mcp_servers_block(rundir, iserror=False):
    """The `mcpServers` map, in the shape Gemini CLI (and so qwen) documents: command/args/env."""
    env = {"S25_MCP_ISERROR": "1"} if iserror else {}
    return {"marion": {"command": sys.executable,
                       "args": [MCP, os.path.join(rundir, "mcp.jsonl")],
                       "env": env}}


def run_scenario(name, out_root, *, settings=None, project_settings=None, argv_extra=(),
                 env_extra=None, prompt=PROMPT, provider_status=200, iserror=False,
                 write_tool="write_file", prompt_flag="-p", resume_from=None, no_openai_env=False,
                 argv_tail=(),
                 openai_env_names=("OPENAI_BASE_URL", "OPENAI_API_KEY", "OPENAI_MODEL")):
    rundir = os.path.join(out_root, name)
    if os.path.exists(rundir):
        shutil.rmtree(rundir)
    home = os.path.join(rundir, "home")
    work = os.path.join(rundir, "work")
    os.makedirs(home)
    os.makedirs(work)

    port = free_port()
    base_url = "http://127.0.0.1:%d/v1" % port
    penv = dict(os.environ, S25_PORT=str(port), S25_REQLOG=os.path.join(rundir, "provider.jsonl"),
                S25_TOOL=MCP_TOOL_NAME, S25_WRITE_TOOL=write_tool,
                S25_WRITE_PATH=os.path.join(work, "s25-note.txt"), S25_STATUS=str(provider_status),
                S25_MODEL=MODEL)
    prov = subprocess.Popen([sys.executable, PROVIDER], env=penv,
                            stderr=open(os.path.join(rundir, "provider.err"), "w"))
    try:
        if not wait_port(port):
            raise SystemExit("provider did not come up")

        if settings is not None:
            doc = settings(rundir, iserror) if callable(settings) else settings
            with open(os.path.join(home, "settings.json"), "w") as fh:
                fh.write(json.dumps(doc, indent=2).replace("{BASE_URL}", base_url))
        if project_settings is not None:
            doc = project_settings(rundir, iserror) if callable(project_settings) else project_settings
            os.makedirs(os.path.join(work, ".qwen"))
            with open(os.path.join(work, ".qwen", "settings.json"), "w") as fh:
                json.dump(doc, fh, indent=2)

        env = {"PATH": "/opt/homebrew/bin:/usr/bin:/bin", "HOME": os.path.expanduser("~"),
               "QWEN_HOME": home, "TERM": "dumb", "NO_COLOR": "1"}
        if not no_openai_env:
            vals = {"OPENAI_BASE_URL": base_url, "OPENAI_API_KEY": API_KEY, "OPENAI_MODEL": MODEL}
            for k in openai_env_names:
                env[k] = vals[k]
        if env_extra:
            for k, v in env_extra.items():
                env[k] = v.replace("{BASE_URL}", base_url) if isinstance(v, str) else v

        argv = [QWEN]
        if resume_from:
            argv += ["--resume", resume_from]
        argv += list(argv_extra)
        if prompt is not None:
            argv += [prompt_flag, prompt] if prompt_flag else [prompt]
        argv += ["--output-format", "stream-json"]
        argv += list(argv_tail)

        with open(os.path.join(rundir, "argv.json"), "w") as fh:
            json.dump(argv, fh, indent=2)
        with open(os.path.join(rundir, "env.json"), "w") as fh:
            json.dump({k: ("<key>" if k == "OPENAI_API_KEY" else v) for k, v in env.items()}, fh,
                      indent=2)

        say("%s: %s" % (name, " ".join(argv[1:])))
        t0 = time.time()
        code = run_bounded(argv, work, env, rundir)
        dt = time.time() - t0
        with open(os.path.join(rundir, "exit.txt"), "w") as fh:
            fh.write("%s\n%.1fs\n" % (code, dt))
        with open(os.path.join(rundir, "home-after.txt"), "w") as fh:
            fh.write(snapshot(home))
        with open(os.path.join(rundir, "work-after.txt"), "w") as fh:
            fh.write(snapshot(work))
        say("%s: exit %s in %.1fs" % (name, code, dt))
        return rundir, code
    finally:
        prov.terminate()
        prov.wait()


def settings_doc(rundir, iserror):
    """Bare: just the MCP declaration. What the first probes ran with."""
    return {"mcpServers": mcp_servers_block(rundir, iserror)}


def settings_doc_quiet(rundir, iserror):
    """The document the adapter would write: MCP plus the switch that stops the hidden
    memory-extraction side turn (measured in `probe-env-only`, request #2)."""
    return {"mcpServers": mcp_servers_block(rundir, iserror),
            "memory": {"enableManagedAutoMemory": False}}


def settings_doc_visible(rundir, iserror):
    """`tools.visible`: the settings-side way to unhide a deferred tool at session start."""
    return {"mcpServers": mcp_servers_block(rundir, iserror),
            "memory": {"enableManagedAutoMemory": False},
            "tools": {"visible": [MCP_TOOL_NAME]}}


SCENARIOS = {}


def scenario(fn):
    SCENARIOS[fn.__name__.replace("_", "-")] = fn
    return fn


@scenario
def probe_env_only(out):
    """No settings.json at all: does qwen run headless on OPENAI_* env alone, and where does it POST?"""
    return run_scenario("probe-env-only", out, prompt="say hi", write_tool="none")


@scenario
def probe_no_openai_env(out):
    """Nothing pointing at a provider: the login/auth refusal, verbatim."""
    return run_scenario("probe-no-openai-env", out, prompt="say hi", write_tool="none",
                        no_openai_env=True)


@scenario
def probe_settings_auth(out):
    """Provider declared in settings.json instead of env (the documented alternative)."""
    return run_scenario("probe-settings-auth", out, prompt="say hi", write_tool="none",
                        no_openai_env=True, settings=lambda r, e: {
                            "security": {"auth": {"selectedType": "openai", "apiKey": API_KEY,
                                                  "baseUrl": "{BASE_URL}"}},
                            "model": {"name": MODEL}})


@scenario
def baseline_yolo(out):
    """MCP declared, --yolo, nothing else: the default built-in tool list, and the MCP tool arriving
    *deferred* — the canned model has to call `tool_search` before it can call the report tool."""
    return run_scenario("baseline-yolo", out, settings=settings_doc, argv_extra=["--yolo"])


@scenario
def blocking_mcp(out):
    """Same, plus QWEN_CODE_LEGACY_MCP_BLOCKING=1: is the MCP tool in `tools[]` on request one?"""
    return run_scenario("blocking-mcp", out, settings=settings_doc_quiet, argv_extra=["--yolo"],
                        env_extra=BLOCKING_MCP)


@scenario
def visible_tools(out):
    """The settings-only alternative: `tools.visible` naming the MCP tool, no env var."""
    return run_scenario("visible-tools", out, settings=settings_doc_visible, argv_extra=["--yolo"])


@scenario
def write_then_report(out):
    """The run the adapter compiles: blocking MCP, memory side-turn off, --yolo, and `--core-tools`
    restricting the model to write_file + the MCP tool."""
    return run_scenario("write-then-report", out, settings=settings_doc_quiet, env_extra=BLOCKING_MCP,
                        argv_extra=["--yolo", "--core-tools", "write_file", MCP_TOOL_NAME])


# The twelve tools `--core-tools` did not remove in `write-then-report` (read off its
# provider.jsonl): exempt from the legacy allowlist, so they need `--exclude-tools`.
CORE_TOOLS_SURVIVORS = ["agent", "enter_worktree", "exit_worktree", "get_goal", "list_agents",
                        "record_artifact", "report_findings", "send_message", "skill", "task_stop",
                        "tool_search", "update_goal"]


@scenario
def core_plus_exclude(out):
    """`--core-tools` plus `--exclude-tools` naming the survivors: can the list be shrunk to two?"""
    return run_scenario("core-plus-exclude", out, settings=settings_doc_quiet, env_extra=BLOCKING_MCP,
                        argv_extra=["--yolo", "--core-tools", "write_file", MCP_TOOL_NAME,
                                    "--exclude-tools", *CORE_TOOLS_SURVIVORS])


@scenario
def bare_mcp_config(out):
    """`--bare` (no settings auto-discovery) with the MCP server passed inline via `--mcp-config`."""
    def argv(rundir):
        return ["--bare", "--yolo", "--core-tools", "write_file", MCP_TOOL_NAME, "--mcp-config",
                json.dumps({"mcpServers": mcp_servers_block(rundir, False)})]
    rundir = os.path.join(out, "bare-mcp-config")
    return run_scenario("bare-mcp-config", out, env_extra=BLOCKING_MCP, argv_extra=argv(rundir))


@scenario
def mcp_config_argv(out):
    """The compiled run, but the server declared ONLY via `--mcp-config '<inline json>'` on argv:
    no `mcpServers` in any settings.json (settings carries just the memory switch), no `--bare`."""
    rundir = os.path.join(out, "mcp-config-argv")
    return run_scenario("mcp-config-argv", out, env_extra=BLOCKING_MCP,
                        settings={"memory": {"enableManagedAutoMemory": False}},
                        argv_extra=["--yolo", "--core-tools", "write_file", MCP_TOOL_NAME,
                                    "--exclude-tools", *CORE_TOOLS_SURVIVORS, "--mcp-config",
                                    json.dumps({"mcpServers": mcp_servers_block(rundir, False)})])


@scenario
def core_tools_empty(out):
    """`mcp-config-argv` but with `--core-tools` given no names: does qwen start, and what does
    request one's `tools[]` hold?"""
    rundir = os.path.join(out, "core-tools-empty")
    return run_scenario("core-tools-empty", out, env_extra=BLOCKING_MCP,
                        settings={"memory": {"enableManagedAutoMemory": False}},
                        argv_extra=["--yolo", "--core-tools", "--exclude-tools", *CORE_TOOLS_SURVIVORS],
                        argv_tail=["--mcp-config",
                                   json.dumps({"mcpServers": mcp_servers_block(rundir, False)})],
                        write_tool="none")


@scenario
def report_iserror(out):
    return run_scenario("report-iserror", out, settings=settings_doc_quiet, env_extra=BLOCKING_MCP,
                        iserror=True, argv_extra=["--yolo", "--core-tools", MCP_TOOL_NAME],
                        write_tool="none")


@scenario
def provider_500(out):
    """Every POST answers 500: how often does qwen retry, and does it ever give up on its own?"""
    return run_scenario("provider-500", out, settings=settings_doc_quiet, env_extra=BLOCKING_MCP,
                        provider_status=500, argv_extra=["--yolo"], write_tool="none")


@scenario
def provider_500_wall_time(out):
    """Same, bounded by qwen's own `--max-wall-time` (help text: exit code 55 when exceeded)."""
    return run_scenario("provider-500-wall-time", out, settings=settings_doc_quiet,
                        env_extra=BLOCKING_MCP, provider_status=500,
                        argv_extra=["--yolo", "--max-wall-time", "30"], write_tool="none")


@scenario
def denied_without_yolo(out):
    """Same as write-then-report but no --yolo / --approval-mode: what happens to the tool calls?"""
    return run_scenario("denied-without-yolo", out, settings=settings_doc_quiet,
                        env_extra=BLOCKING_MCP,
                        argv_extra=["--core-tools", "write_file", MCP_TOOL_NAME])


@scenario
def project_settings(out):
    """MCP declared in the cwd's .qwen/settings.json instead of QWEN_HOME."""
    return run_scenario("project-settings", out, project_settings=settings_doc_quiet,
                        env_extra=BLOCKING_MCP,
                        argv_extra=["--yolo", "--core-tools", MCP_TOOL_NAME], write_tool="none")


@scenario
def resume(out):
    """Turn one, then `--resume <id>` with the id read off turn one's stream."""
    r1, code = run_scenario("resume-turn-1", out, settings=settings_doc_quiet,
                            env_extra=BLOCKING_MCP,
                            argv_extra=["--yolo", "--core-tools", MCP_TOOL_NAME], write_tool="none")
    sid = None
    with open(os.path.join(r1, "stdout.jsonl")) as fh:
        for line in fh:
            try:
                f = json.loads(line)
            except ValueError:
                continue
            sid = sid or f.get("session_id") or f.get("sessionId")
    say("resume: session id from turn 1 = %r" % sid)
    if not sid:
        return r1, code
    rundir = os.path.join(out, "resume-turn-2")
    if os.path.exists(rundir):
        shutil.rmtree(rundir)
    os.makedirs(rundir)
    # Turn two must run with turn one's QWEN_HOME *and* cwd: sessions live under
    # `$QWEN_HOME/projects/<cwd-slug>/chats/<session_id>.jsonl`, so a copied tree under a new path
    # is a different project and `--resume` answers "No saved session found" (first attempt).
    return _rerun_in(rundir, sid, os.path.join(r1, "home"), os.path.join(r1, "work"))


def _rerun_in(rundir, sid, home, work):
    port = free_port()
    base_url = "http://127.0.0.1:%d/v1" % port
    penv = dict(os.environ, S25_PORT=str(port), S25_REQLOG=os.path.join(rundir, "provider.jsonl"),
                S25_TOOL=MCP_TOOL_NAME, S25_WRITE_TOOL="none", S25_MODEL=MODEL)
    prov = subprocess.Popen([sys.executable, PROVIDER], env=penv,
                            stderr=open(os.path.join(rundir, "provider.err"), "w"))
    try:
        wait_port(port)
        # settings.json points the MCP transcript at turn one's dir; repoint it here.
        sp = os.path.join(home, "settings.json")
        doc = json.load(open(sp))
        doc["mcpServers"]["marion"]["args"][1] = os.path.join(rundir, "mcp.jsonl")
        json.dump(doc, open(sp, "w"), indent=2)
        env = {"PATH": "/opt/homebrew/bin:/usr/bin:/bin", "HOME": os.path.expanduser("~"),
               "QWEN_HOME": home, "TERM": "dumb", "NO_COLOR": "1", "OPENAI_BASE_URL": base_url,
               "OPENAI_API_KEY": API_KEY, "OPENAI_MODEL": MODEL, **BLOCKING_MCP}
        argv = [QWEN, "--resume", sid, "--yolo", "--core-tools", MCP_TOOL_NAME, "-p",
                "Second turn: call the report tool once more, narrative 'turn two'.",
                "--output-format", "stream-json"]
        json.dump(argv, open(os.path.join(rundir, "argv.json"), "w"), indent=2)
        json.dump({k: ("<key>" if k == "OPENAI_API_KEY" else v) for k, v in env.items()},
                  open(os.path.join(rundir, "env.json"), "w"), indent=2)
        say("resume-turn-2: %s" % " ".join(argv[1:]))
        t0 = time.time()
        with open(os.path.join(rundir, "stdout.jsonl"), "wb") as so, \
                open(os.path.join(rundir, "stderr.txt"), "wb") as se:
            try:
                code = subprocess.run(argv, cwd=work, env=env, stdin=subprocess.DEVNULL, stdout=so,
                                      stderr=se, timeout=TIMEOUT).returncode
            except subprocess.TimeoutExpired:
                code = "timeout-%ds" % TIMEOUT
        open(os.path.join(rundir, "exit.txt"), "w").write("%s\n%.1fs\n" % (code, time.time() - t0))
        open(os.path.join(rundir, "home-after.txt"), "w").write(snapshot(home))
        say("resume-turn-2: exit %s" % code)
        return rundir, code
    finally:
        prov.terminate()
        prov.wait()


def main():
    args = sys.argv[1:]
    out = os.path.abspath(os.getenv("S25_OUT", "/tmp/marion-s25"))
    if "--out" in args:
        i = args.index("--out")
        out = os.path.abspath(args[i + 1])
        del args[i:i + 2]
    names = list(SCENARIOS) if args == ["all"] else args
    os.makedirs(out, exist_ok=True)
    qh = os.path.expanduser("~/.qwen")
    before = os.stat(qh).st_mtime if os.path.exists(qh) else None
    for n in names:
        SCENARIOS[n](out)
    after = os.stat(qh).st_mtime if os.path.exists(qh) else None
    say("~/.qwen mtime before=%r after=%r" % (before, after))


if __name__ == "__main__":
    main()
