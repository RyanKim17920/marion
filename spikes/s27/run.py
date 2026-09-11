#!/usr/bin/env python3
"""S27 — Cline CLI 3.0.61 against a canned local provider, no network, no credential.

One invocation = one run directory under <outroot>/<label>/ holding:

    cfg/            CLINE_DIR             (config root; default ~/.cline)
    data/           CLINE_DATA_DIR        (state root; default <CLINE_DIR>/data)
      settings/providers.json             the provider selection, byte-for-byte what
                                          `cline auth openai-compatible -k -m -b` wrote
      settings/cline_mcp_settings.json    the MCP declaration, shape from `cline mcp install --yes`
    home/           HOME                  (rules/skills dirs under ~ are read from here)
    work/           -c / cwd
    stdout.jsonl    the --json stream
    stderr.txt
    provider-requests.jsonl               every POST the stub saw (Authorization redacted)
    mcp.jsonl                             the MCP stub's transcript
    history.json                          `cline history --json` run afterwards in the same tree
    meta.json                             argv, env, rc, timings, ~/.cline mtimes before/after,
                                          `pgrep -fl cline` before/after, data/ tree after

Options:
    --iserror            MCP stub answers tools/call with isError: true
    --fail 500           provider answers every POST with that HTTP status
    --no-auto-approve    pass --auto-approve false (measure the denied path)
    --reuse <label>      reuse that run's cfg/data/home/work (for --id resume)
    --id <session-id>    pass through to cline
    --tool <name>        the tools[] name the stub must see before emitting a call
    --timeout <secs>     kill cline after this long (default 120)
    --prompt <text>
    --no-json            omit --json
    --flags-not-env      use --config/--data-dir flags instead of the env vars
    --flags-and-env      env vars and flags together (the combination the adapter should use)
    --external-mcp       MCP document outside data/, via CLINE_MCP_SETTINGS_PATH
    --external-providers providers.json outside data/, via CLINE_PROVIDER_SETTINGS_PATH
    --model <id>         add `-m <id>` to argv (does the flag beat providers.json?)
    --global-settings J  write J to data/settings/global-settings.json before the run

Usage: run.py <label> [options]
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
OUTROOT = os.getenv("S27_OUTROOT") or os.path.join(
    tempfile.gettempdir(), "marion-s27")
PORT = int(os.getenv("S27_PORT", "8127"))
BASE_URL = "http://127.0.0.1:%d/v1" % PORT
API_KEY = "marion-canned-credential-27cc"   # minted here; never a real credential
PROVIDER, MODEL = "openai-compatible", "canned-1"
REAL_HOME = os.path.expanduser("~")


def say(msg):
    sys.stderr.write("[s27] %s\n" % msg)
    sys.stderr.flush()


def providers_json():
    """Exactly what `cline auth openai-compatible -k … -m … -b …` wrote (auth_probe.py),
    with the timestamp it stamped kept as a fixed string."""
    return {
        "version": 1,
        "lastUsedProvider": PROVIDER,
        "modes": {},
        "providers": {
            PROVIDER: {
                "settings": {
                    "provider": PROVIDER,
                    "apiKey": API_KEY,
                    "model": MODEL,
                    "baseUrl": BASE_URL,
                },
                "updatedAt": "2026-09-05T00:00:00.000Z",
                "tokenSource": "manual",
            }
        },
    }


def mcp_settings(mcp_log):
    """Exactly the shape `cline mcp install marion --yes -- python3 … ` wrote (mcp_probe.py)."""
    return {
        "mcpServers": {
            "marion": {
                "transport": {
                    "type": "stdio",
                    "command": sys.executable,
                    "args": [os.path.join(HERE, "mcp_report_server.py"), mcp_log],
                }
            }
        }
    }


def wait_port(port, secs=15):
    end = time.time() + secs
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), 1):
                return True
        except OSError:
            time.sleep(0.2)
    return False


def home_mtimes():
    d = os.path.join(REAL_HOME, ".cline")
    out = {}
    if not os.path.isdir(d):
        return out
    for root, dirs, files in os.walk(d):
        out[root] = os.stat(root).st_mtime
        for f in files:
            p = os.path.join(root, f)
            out[p] = os.stat(p).st_mtime
    return out


def cline_procs():
    p = subprocess.run(["pgrep", "-fl", "cline"], capture_output=True, text=True)
    return [l for l in p.stdout.splitlines() if "run.py" not in l and "pgrep" not in l]


def tree(root):
    out = []
    for r, _, files in os.walk(root):
        for f in files:
            p = os.path.join(r, f)
            out.append({"path": os.path.relpath(p, root), "size": os.path.getsize(p)})
    return sorted(out, key=lambda x: x["path"])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("label")
    ap.add_argument("--iserror", action="store_true")
    ap.add_argument("--fail", type=int, default=0)
    ap.add_argument("--no-auto-approve", action="store_true")
    ap.add_argument("--reuse")
    ap.add_argument("--id")
    ap.add_argument("--tool", default=os.getenv("S27_TOOL", "marion__report"))
    ap.add_argument("--timeout", type=int, default=120)
    ap.add_argument("--prompt", default="Call the marion report tool with a one-line narrative. Do nothing else.")
    ap.add_argument("--extra", nargs="*", default=[], help="extra cline argv")
    ap.add_argument("--stdin-prompt", action="store_true", help="pipe the prompt on stdin instead")
    ap.add_argument("--no-json", action="store_true", help="omit --json (styled text output)")
    ap.add_argument("--flags-not-env", action="store_true",
                    help="relocate with --config/--data-dir flags instead of CLINE_DIR/CLINE_DATA_DIR")
    ap.add_argument("--external-mcp", action="store_true",
                    help="write cline_mcp_settings.json OUTSIDE data/ and point CLINE_MCP_SETTINGS_PATH at it")
    ap.add_argument("--external-providers", action="store_true",
                    help="write providers.json OUTSIDE data/ and point CLINE_PROVIDER_SETTINGS_PATH at it")
    ap.add_argument("--model", help="pass -m <id> before the positional prompt")
    ap.add_argument("--flags-and-env", action="store_true",
                    help="both: CLINE_DIR/CLINE_DATA_DIR env AND --config/--data-dir flags")
    ap.add_argument("--global-settings", help="JSON written to data/settings/global-settings.json")
    ap.add_argument("--prompt-first", action="store_true", help="positional prompt before the flags")
    a = ap.parse_args()

    run = os.path.join(OUTROOT, a.label)
    if a.reuse:
        src = os.path.join(OUTROOT, a.reuse)
        if os.path.exists(run):
            shutil.rmtree(run)
        shutil.copytree(src, run)
        for f in ("stdout.jsonl", "stderr.txt", "provider-requests.jsonl", "mcp.jsonl", "meta.json", "history.json"):
            try:
                os.remove(os.path.join(run, f))
            except FileNotFoundError:
                pass
    else:
        shutil.rmtree(run, ignore_errors=True)
        for d in ("cfg", "data/settings", "home", "work"):
            os.makedirs(os.path.join(run, d))

    mcp_log = os.path.join(run, "mcp.jsonl")
    ext = os.path.join(run, "external")
    os.makedirs(ext, exist_ok=True)
    prov_path = os.path.join(ext if a.external_providers else os.path.join(run, "data", "settings"),
                             "providers.json")
    mcp_path = os.path.join(ext if a.external_mcp else os.path.join(run, "data", "settings"),
                            "cline_mcp_settings.json")
    with open(prov_path, "w") as fh:
        json.dump(providers_json(), fh, indent=2)
        fh.write("\n")
    with open(mcp_path, "w") as fh:
        json.dump(mcp_settings(mcp_log), fh, indent=2)
        fh.write("\n")

    if a.global_settings:
        with open(os.path.join(run, "data", "settings", "global-settings.json"), "w") as fh:
            fh.write(a.global_settings + "\n")

    reqlog = os.path.join(run, "provider-requests.jsonl")
    penv = dict(os.environ, S27_PORT=str(PORT), S27_REQLOG=reqlog, S27_TOOL=a.tool,
                S27_FAIL_STATUS=str(a.fail))
    prov = subprocess.Popen([sys.executable, os.path.join(HERE, "canned_provider.py")],
                            env=penv, stderr=subprocess.DEVNULL)
    if not wait_port(PORT):
        prov.kill()
        say("provider never listened")
        return 2

    env = {k: os.environ[k] for k in ("PATH", "TMPDIR", "LANG", "TERM") if k in os.environ}
    env["HOME"] = os.path.join(run, "home")
    if not a.flags_not_env:
        env["CLINE_DIR"] = os.path.join(run, "cfg")
        env["CLINE_DATA_DIR"] = os.path.join(run, "data")
    if a.iserror:
        env["S27_MCP_ISERROR"] = "1"   # inherited by the MCP child through cline
    if a.external_mcp:
        env["CLINE_MCP_SETTINGS_PATH"] = mcp_path
    if a.external_providers:
        env["CLINE_PROVIDER_SETTINGS_PATH"] = prov_path

    argv = ["cline", "-c", os.path.join(run, "work")] if a.no_json else \
        ["cline", "--json", "-c", os.path.join(run, "work")]
    if a.prompt_first and not a.stdin_prompt:
        argv.insert(1, a.prompt)
    if a.flags_not_env or a.flags_and_env:
        argv += ["--config", os.path.join(run, "cfg"), "--data-dir", os.path.join(run, "data")]
    if a.no_auto_approve:
        argv += ["--auto-approve", "false"]
    if a.id:
        argv += ["--id", a.id]
    if a.model:
        argv += ["-m", a.model]
    argv += a.extra
    if not a.stdin_prompt and not a.prompt_first:
        argv.append(a.prompt)

    before_home, before_procs = home_mtimes(), cline_procs()
    say("argv: %s" % " ".join(argv))
    t0 = time.time()
    timed_out = False
    with open(os.path.join(run, "stdout.jsonl"), "wb") as so, \
            open(os.path.join(run, "stderr.txt"), "wb") as se:
        child = subprocess.Popen(argv, env=env, cwd=os.path.join(run, "work"),
                                 stdin=subprocess.PIPE if a.stdin_prompt else subprocess.DEVNULL,
                                 stdout=so, stderr=se)
        if a.stdin_prompt:
            child.stdin.write(a.prompt.encode())
            child.stdin.close()
        try:
            rc = child.wait(timeout=a.timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            child.kill()
            rc = child.wait()
    elapsed = time.time() - t0
    time.sleep(1.0)
    after_home, after_procs = home_mtimes(), cline_procs()

    # Where does a session id come from? Ask the CLI itself, in the same isolated tree.
    hist = subprocess.run(["cline", "history", "--json"], env=env, cwd=os.path.join(run, "work"),
                          stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
    with open(os.path.join(run, "history.json"), "w") as fh:
        fh.write(hist.stdout)

    try:
        prov.terminate()
        prov.wait(timeout=5)
    except Exception:
        prov.kill()

    meta = {
        "label": a.label, "argv": argv, "stdin_prompt": a.prompt if a.stdin_prompt else None, "env": env, "cwd": os.path.join(run, "work"),
        "rc": rc, "timed_out": timed_out, "elapsed_s": round(elapsed, 2),
        "home_cline_mtimes_before": before_home, "home_cline_mtimes_after": after_home,
        "home_cline_changed": before_home != after_home,
        "cline_procs_before": before_procs, "cline_procs_after": after_procs,
        "data_tree_after": tree(os.path.join(run, "data")),
        "cfg_tree_after": tree(os.path.join(run, "cfg")),
        "home_tree_after": tree(os.path.join(run, "home")),
        "work_tree_after": tree(os.path.join(run, "work")),
    }
    with open(os.path.join(run, "meta.json"), "w") as fh:
        json.dump(meta, fh, indent=2)
    say("rc=%s timed_out=%s elapsed=%.1fs home_changed=%s procs_after=%d"
        % (rc, timed_out, elapsed, meta["home_cline_changed"], len(after_procs)))
    say("run dir: %s" % run)
    return 0


if __name__ == "__main__":
    sys.exit(main())
