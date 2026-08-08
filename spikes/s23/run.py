#!/usr/bin/env python3
"""S23 — `opencode acp` against a canned local provider, no network, no credential.

S21 drove `opencode acp` to a `marion_report` call, but on the operator's live login and a real
model. S13 drove `opencode run` against a canned OpenAI-compatible endpoint at $0.00. Neither
answers the question a `HarnessAdapter` test needs: **do the two compose?** — does the ACP surface
honour the same `$XDG_CONFIG_HOME` document the `run` surface honours, so that marion's ACP node
can be exercised in CI with no vendor on the far end.

What this wires together:

  * `spikes/s23/canned_provider.py` on 127.0.0.1 — the model.
  * `spikes/s21/mcp_echo_server.py` — marion's stand-in bridge, offering exactly `report`,
    declared over ACP's `session/new` (**not** in the config document; the bridge rides the
    protocol, which is the whole difference between this surface and `run`).
  * `spikes/s21/acp_child_probe.py` — the ACP client, reused verbatim.

The config document is byte-identical to what `marion_harness::opencode::config_json` emits for
this `ConfigSpec` with `mcp: None` — see `CONFIG_NOTE`. It is hand-written rather than dumped from
the crate because the spike must not add code to `crates/`.

`S23_DEAD_PROXY=1` additionally points `HTTP_PROXY`/`HTTPS_PROXY` at a closed loopback port with
`NO_PROXY=127.0.0.1`, so **any** off-loopback HTTP the child attempted would fail. A run that
completes identically under it has proven that nothing off-loopback was load-bearing. It does not
prove zero packets left the host — a swallowed, non-load-bearing attempt would look the same — and
the README says so rather than overclaiming.

Usage: run.py [outdir]   (default: tests/fixtures/s23)
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
S21 = os.path.join(ROOT, "spikes", "s21")
OUT = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else os.path.join(
    ROOT, "tests", "fixtures", "s23")

PORT = int(os.getenv("S23_PORT", "8123"))
BASE_URL = "http://127.0.0.1:%d/v1" % PORT
API_KEY = "sk-marion-s23-canned"          # minted here; never a real credential
PROVIDER, MODEL = "canned", "canned-1"

SANDBOX = os.getenv("S23_SANDBOX", "/tmp/marion-s23/sandbox")
PROBE_CWD = os.getenv("S23_CWD", "/tmp/marion-s23/work")

CONFIG_NOTE = (
    "byte-identical to marion_harness::opencode::config_json(&ConfigSpec{model: canned/canned-1, "
    "base_url: %s, api_key: Some(%s)}, None); keys sorted because serde_json without "
    "preserve_order emits a BTreeMap" % (BASE_URL, "sk-marion-s23-canned")
)


def config_document():
    """`opencode::config_json` with `mcp: None`, transcribed key for key.

    `PROVIDER_TIMEOUT_MS` = 120_000 and `PROVIDER_HEADER_TIMEOUT_MS` = 30_000 are the crate's
    constants. `tool_call: true` on the model entry is what lets the canned turn emit a
    `tool_calls` delta at all. No `mcp` block: on this surface the declaration travels in
    `session/new`.
    """
    return {
        "model": "%s/%s" % (PROVIDER, MODEL),
        "small_model": "%s/%s" % (PROVIDER, MODEL),
        "provider": {
            PROVIDER: {
                "npm": "@ai-sdk/openai-compatible",
                "name": PROVIDER,
                "options": {
                    "baseURL": BASE_URL,
                    "timeout": 120000,
                    "headerTimeout": 30000,
                    "apiKey": API_KEY,
                },
                "models": {MODEL: {"name": MODEL, "tool_call": True}},
            }
        },
    }


def isolation_env(sandbox):
    """`opencode::isolation_env(sandbox, Auth::Canned)`, transcribed."""
    return {
        "HOME": sandbox,
        "XDG_CONFIG_HOME": os.path.join(sandbox, "config"),
        "XDG_DATA_HOME": os.path.join(sandbox, "data"),
        "XDG_CACHE_HOME": os.path.join(sandbox, "cache"),
        "XDG_STATE_HOME": os.path.join(sandbox, "state"),
        "OPENCODE_DISABLE_CLAUDE_CODE": "1",
        "OPENCODE_DISABLE_EXTERNAL_SKILLS": "1",
        "OPENCODE_DISABLE_PROJECT_CONFIG": "1",
        "OPENCODE_DISABLE_MODELS_FETCH": "1",
        "OPENCODE_DISABLE_LSP_DOWNLOAD": "1",
        "OPENCODE_DISABLE_AUTOUPDATE": "1",
        "OPENCODE_DISABLE_SHARE": "1",
        "OPENCODE_DB": ":memory:",
    }


def say(msg):
    sys.stderr.write("[s23] %s\n" % msg)
    sys.stderr.flush()


def wait_port(port, secs=15):
    end = time.time() + secs
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), 1):
                return True
        except OSError:
            time.sleep(0.2)
    return False


def main():
    for d in (SANDBOX, PROBE_CWD):
        shutil.rmtree(d, ignore_errors=True)
    cfgdir = os.path.join(SANDBOX, "config", "opencode")
    os.makedirs(cfgdir)
    os.makedirs(PROBE_CWD)
    os.makedirs(OUT, exist_ok=True)

    cfg_path = os.path.join(cfgdir, "opencode.json")
    with open(cfg_path, "w") as fh:
        json.dump(config_document(), fh, sort_keys=True, separators=(",", ":"))
    say("config written: %s" % cfg_path)

    reqlog = os.path.join(PROBE_CWD, "provider-requests.jsonl")
    penv = dict(os.environ, S23_PORT=str(PORT), S23_REQLOG=reqlog)
    say("starting canned provider on 127.0.0.1:%d" % PORT)
    prov = subprocess.Popen([sys.executable, os.path.join(HERE, "canned_provider.py")],
                            env=penv, stderr=subprocess.PIPE, text=True)
    if not wait_port(PORT):
        prov.kill()
        say("provider never listened")
        return 2

    env = {k: os.environ[k] for k in ("PATH", "TMPDIR", "LANG", "TERM") if k in os.environ}
    env.update(isolation_env(SANDBOX))
    env["PROBE_CWD"] = PROBE_CWD
    if os.getenv("S23_DEAD_PROXY") == "1":
        env.update({"HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
                    "http_proxy": "http://127.0.0.1:1", "https_proxy": "http://127.0.0.1:1",
                    "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost"})
        say("dead-proxy mode: off-loopback HTTP cannot succeed")

    say("driving `opencode acp` — initialize/session_new/session_prompt, up to ~4.5 min")
    t0 = time.time()
    probe = subprocess.run(
        [sys.executable, os.path.join(S21, "acp_child_probe.py"), "opencode-canned",
         "--prompt", "Call the marion report tool. Do nothing else.",
         "--", "opencode", "acp"],
        env=env, capture_output=True, text=True, timeout=600)
    say("probe finished in %.1fs rc=%d" % (time.time() - t0, probe.returncode))

    try:
        prov.terminate()
        prov.wait(timeout=5)
    except Exception:
        prov.kill()

    try:
        result = json.loads(probe.stdout)
    except json.JSONDecodeError:
        say("probe emitted no JSON; stdout head:\n%s\nstderr:\n%s"
            % (probe.stdout[:2000], probe.stderr[-4000:]))
        return 3

    base = os.path.join(OUT, os.getenv("S23_PREFIX", "opencode-acp-canned"))
    with open(base + "-session.jsonl", "w") as fh:
        for line in result["frames"]:
            fh.write(line + "\n")
    with open(base + "-mcp.jsonl", "w") as fh:
        for rec in result["mcp"]:
            fh.write(json.dumps(rec) + "\n")
    with open(base + "-prompt-response.json", "w") as fh:
        json.dump(result["prompt_response"], fh, indent=2)
        fh.write("\n")
    with open(os.path.join(OUT, "opencode.json"), "w") as fh:
        json.dump(config_document(), fh, sort_keys=True, separators=(",", ":"))
        fh.write("\n")
    if os.path.exists(reqlog):
        shutil.copyfile(reqlog, base + "-provider-requests.jsonl")
    with open(base + "-stderr.txt", "w") as fh:
        fh.write("\n".join(result.get("stderr") or []) + "\n")

    resp = result.get("prompt_response") or {}
    stop = (resp.get("result") or {}).get("stopReason")
    called = [r for r in result["mcp"]
              if r.get("dir") == "in" and r.get("frame", {}).get("method") == "tools/call"]
    say("session=%s stopReason=%r tools/call=%d"
        % (result.get("session"), stop, len(called)))
    say("captures in %s" % OUT)
    return 0


if __name__ == "__main__":
    sys.exit(main())
