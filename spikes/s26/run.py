#!/usr/bin/env python3
"""S26 — `goose run` against a canned local provider, no network, no credential.

Drives goose 1.49.0 headless (`goose run -t ... --output-format stream-json`) with its `openai`
provider pointed at `spikes/s26/canned_provider.py` on loopback and one stdio MCP extension,
`spikes/s26/mcp_report_server.py`, declared on the command line with `--with-extension`. Every
scenario is bounded by `timeout 90` so nothing can hang the measurer.

Scenarios (`run.py <scenario>...` or `run.py all`):

  report            the run the adapter compiles: no profile, no session, GOOSE_MODE=auto,
                    marion + developer builtin; canned model calls marion__report -> MCP -> text
  report-noquiet    same without -q, to see what -q suppresses
  iserror           MCP stub answers tools/call with isError: true
  provider-500      provider answers every POST with HTTP 500
  default-mode      GOOSE_MODE unset (goose's default `smart_approve`): does headless run ask, deny, hang?
  chat-mode         GOOSE_MODE=chat: tools withheld from the model?
  builtins-default  no --no-profile, empty GOOSE_CONFIG_DIR: which extensions does goose load anyway?
  with-builtin-dev  --no-profile --with-builtin developer: the developer tool names
  approve-mode      GOOSE_MODE=approve with stdin closed: prompt? deny? hang?
  config-dir-chat   GOOSE_MODE unset, `GOOSE_MODE: chat` written to $GOOSE_CONFIG_DIR/config.yaml:
                    proves whether GOOSE_CONFIG_DIR is where config.yaml is read from
  base-path         OPENAI_BASE_PATH=custom/chat/completions: the exact path the stub receives
  host-v1           OPENAI_HOST ending in /v1: doubled or not?
  session-first     -n s26-sess, no --no-session: session id frames?
  session-resume-redeclare  --resume -n s26-sess with --with-extension marion repeated (fails)
  session-resume    --resume -n s26-sess, extension restored from the session store
  session-id-resume --resume --session-id <id from `goose session list --format json`>
  isolation-xdg     report under HOME=<sandbox> XDG_*_HOME=<sandbox>/...: which dirs still get written

Each scenario writes a run dir under $S26_SCRATCH/<scenario>/ with stdout.jsonl, stderr.txt,
provider-requests.jsonl, mcp.jsonl, meta.json (argv, env minus the key, exit code, duration,
state-dir files created/modified during the run). `redact.py` turns run dirs into fixtures.

The API key is `marion-canned-credential-26bb`, minted here, never a real credential.
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
GOOSE = os.getenv("S26_GOOSE", "/opt/homebrew/bin/goose")
SCRATCH = os.getenv("S26_SCRATCH", "/tmp/marion-s26")
API_KEY = "marion-canned-credential-26bb"
MODEL = "canned-1"
PROMPT = "Call the marion report tool. Do nothing else."
TIMEOUT_S = 90

HOME = os.path.expanduser("~")
STATE_DIRS = [os.path.join(HOME, p) for p in (
    ".config/goose", ".local/state/goose", ".local/share/goose", ".cache/goose",
    "Library/Application Support/goose")]


def say(msg):
    sys.stderr.write("[s26] %s\n" % msg)
    sys.stderr.flush()


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_port(port, secs=15):
    end = time.time() + secs
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), 1):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def snapshot(dirs):
    out = {}
    for d in dirs:
        for dp, _dn, fns in os.walk(d):
            for f in fns:
                p = os.path.join(dp, f)
                try:
                    st = os.stat(p)
                    out[p] = (st.st_mtime_ns, st.st_size)
                except OSError:
                    pass
    return out


def diff_snapshot(before, after):
    created = sorted(p for p in after if p not in before)
    modified = sorted(p for p in after if p in before and after[p] != before[p])
    return {"created": created, "modified": modified}


def base_env(rundir, cfgdir, port):
    env = {k: os.environ[k] for k in ("PATH", "TMPDIR", "LANG", "TERM") if k in os.environ}
    env.update({
        "HOME": HOME,
        "GOOSE_CONFIG_DIR": cfgdir,
        "GOOSE_PROVIDER": "openai",
        "GOOSE_MODEL": MODEL,
        "OPENAI_API_KEY": API_KEY,
        "OPENAI_HOST": "http://127.0.0.1:%d" % port,
        "GOOSE_MODE": "auto",
    })
    return env


def marion_ext(rundir, iserror=False):
    parts = ["marion:S26_MCP_LOG=%s" % os.path.join(rundir, "mcp.jsonl")]
    if iserror:
        parts.append("S26_MCP_ISERROR=1")
    parts += ["python3", os.path.join(HERE, "mcp_report_server.py")]
    return " ".join(parts)


def run_scenario(name, args_fn, env_fn=None, provider_env=None, keep_store=None):
    rundir = os.path.join(SCRATCH, name)
    shutil.rmtree(rundir, ignore_errors=True)
    cfgdir = os.path.join(rundir, "cfg")
    work = os.path.join(rundir, "work")
    os.makedirs(cfgdir)
    os.makedirs(work)

    port = free_port()
    reqlog = os.path.join(rundir, "provider-requests.jsonl")
    penv = dict(os.environ, S26_PORT=str(port), S26_REQLOG=reqlog, S26_MODEL=MODEL)
    penv.update(provider_env or {})
    prov = subprocess.Popen([sys.executable, os.path.join(HERE, "canned_provider.py")],
                            env=penv, stderr=open(os.path.join(rundir, "provider.stderr"), "w"))
    if not wait_port(port):
        prov.kill()
        raise SystemExit("provider never listened")

    env = base_env(rundir, cfgdir, port)
    if env_fn:
        env_fn(env, rundir)
    argv = ["timeout", str(TIMEOUT_S), GOOSE, "run"] + args_fn(rundir)

    watch = STATE_DIRS + [rundir]
    before = snapshot(watch)
    say("%s: %s" % (name, " ".join(argv[3:])))
    t0 = time.time()
    with open(os.path.join(rundir, "stdout.jsonl"), "w") as so, \
            open(os.path.join(rundir, "stderr.txt"), "w") as se:
        rc = subprocess.run(argv, cwd=work, env=env, stdin=subprocess.DEVNULL,
                            stdout=so, stderr=se).returncode
    dt = time.time() - t0
    after = snapshot(watch)

    prov.terminate()
    try:
        prov.wait(timeout=5)
    except Exception:
        prov.kill()

    safe_env = {k: ("<redacted>" if k == "OPENAI_API_KEY" else v) for k, v in env.items()
                if k not in ("PATH", "TMPDIR", "LANG", "TERM")}
    meta = {
        "scenario": name,
        "goose_version": subprocess.run([GOOSE, "--version"], capture_output=True,
                                        text=True).stdout.strip(),
        "argv": argv[3:],
        "env": safe_env,
        "cwd": work,
        "exit_code": rc,
        "timed_out": rc == 124,
        "duration_s": round(dt, 2),
        "provider_port": port,
        "state_dirs": diff_snapshot(before, after),
    }
    with open(os.path.join(rundir, "meta.json"), "w") as fh:
        json.dump(meta, fh, indent=2)
        fh.write("\n")
    say("%s: exit=%d in %.1fs; state files created=%d modified=%d"
        % (name, rc, dt, len(meta["state_dirs"]["created"]), len(meta["state_dirs"]["modified"])))
    return meta


COMMON = ["-t", PROMPT, "--output-format", "stream-json", "-q"]


def sc_report(rundir):
    return COMMON + ["--no-session", "--no-profile", "--with-builtin", "developer",
                     "--with-extension", marion_ext(rundir)]


def sc_report_noquiet(rundir):
    return [a for a in sc_report(rundir) if a != "-q"]


def sc_iserror(rundir):
    return COMMON + ["--no-session", "--no-profile",
                     "--with-extension", marion_ext(rundir, iserror=True)]


def sc_marion_only(rundir):
    return COMMON + ["--no-session", "--no-profile", "--with-extension", marion_ext(rundir)]


def sc_builtins_default(rundir):
    return COMMON + ["--no-session", "--with-extension", marion_ext(rundir)]


def sc_with_builtin_dev(rundir):
    return COMMON + ["--no-session", "--no-profile", "--with-builtin", "developer",
                     "--with-extension", marion_ext(rundir)]


def sc_session_first(rundir):
    return COMMON + ["-n", "s26-sess", "--no-profile", "--with-extension", marion_ext(rundir)]


def sc_session_resume_redeclare(rundir):
    return ["-t", "Say the word done.", "--output-format", "stream-json", "-q",
            "--resume", "-n", "s26-sess", "--no-profile", "--with-extension", marion_ext(rundir)]


def sc_session_resume(rundir):
    # no --with-extension: goose restores the extension list from the session store
    return ["-t", "Say the word done.", "--output-format", "stream-json", "-q",
            "--resume", "-n", "s26-sess", "--no-profile"]


def session_id_by_name(name, cfgdir):
    out = subprocess.run([GOOSE, "session", "list", "--format", "json"], capture_output=True,
                         text=True, env=dict(os.environ, GOOSE_CONFIG_DIR=cfgdir)).stdout
    for sess in json.loads(out or "[]"):
        if sess.get("name") == name:
            return sess["id"]
    return ""


def sc_session_id_resume(rundir):
    sid = session_id_by_name("s26-sess", os.path.join(rundir, "cfg"))
    say("session id from `goose session list --format json`: %r" % sid)
    return ["-t", "Say the word done again.", "--output-format", "stream-json", "-q",
            "--resume", "--session-id", sid, "--no-profile"]


def env_no_mode(env, rundir):
    env.pop("GOOSE_MODE", None)


def env_chat_mode(env, rundir):
    env["GOOSE_MODE"] = "chat"


def env_approve_mode(env, rundir):
    env["GOOSE_MODE"] = "approve"


def env_config_dir_chat(env, rundir):
    env.pop("GOOSE_MODE", None)
    with open(os.path.join(env["GOOSE_CONFIG_DIR"], "config.yaml"), "w") as fh:
        fh.write("GOOSE_MODE: chat\n")


def env_base_path(env, rundir):
    env["OPENAI_BASE_PATH"] = "custom/chat/completions"


def env_host_v1(env, rundir):
    env["OPENAI_HOST"] = env["OPENAI_HOST"] + "/v1"


def sc_env_inherit(rundir):
    # no ENV=v in the declaration; the log path is passed as argv[1] as a fallback so a
    # transcript exists whether or not the child inherits goose's env (env wins in the stub)
    return COMMON + ["--no-session", "--no-profile", "--with-extension",
                     "marion:python3 %s %s" % (os.path.join(HERE, "mcp_report_server.py"),
                                               os.path.join(rundir, "mcp-argv.jsonl"))]


def env_inherit(env, rundir):
    env["S26_MCP_LOG"] = os.path.join(rundir, "mcp.jsonl")
    env["S26_INHERITED"] = "yes"


def env_isolated(env, rundir):
    sb = os.path.join(rundir, "sandbox")
    env.update({
        "HOME": sb,
        "XDG_CONFIG_HOME": os.path.join(sb, "config"),
        "XDG_DATA_HOME": os.path.join(sb, "data"),
        "XDG_STATE_HOME": os.path.join(sb, "state"),
        "XDG_CACHE_HOME": os.path.join(sb, "cache"),
    })
    os.makedirs(sb, exist_ok=True)


def env_isolated_chat(env, rundir):
    """Isolated like above, GOOSE_MODE unset in the env and `GOOSE_MODE: chat` in
    $XDG_CONFIG_HOME/goose/config.yaml. A skipped tool call proves that file is read."""
    env_isolated(env, rundir)
    env.pop("GOOSE_MODE", None)
    cfg = os.path.join(env["XDG_CONFIG_HOME"], "goose")
    os.makedirs(cfg, exist_ok=True)
    with open(os.path.join(cfg, "config.yaml"), "w") as fh:
        fh.write("GOOSE_MODE: chat\n")


def info_paths(outdir):
    """`goose info` under five env shapes: where does goose say config/sessions/logs live?"""
    sb = os.path.join(outdir, "info-sandbox")
    base = {k: os.environ[k] for k in ("PATH", "TMPDIR", "LANG", "TERM") if k in os.environ}
    base.update(HOME=HOME, GOOSE_PROVIDER="openai", GOOSE_MODEL=MODEL, OPENAI_API_KEY=API_KEY)
    cases = [
        ("no relocation", {}),
        ("GOOSE_CONFIG_DIR=<SB>/gcd", {"GOOSE_CONFIG_DIR": sb + "/gcd"}),
        ("XDG_CONFIG_HOME=<SB>/xc", {"XDG_CONFIG_HOME": sb + "/xc"}),
        ("HOME=<SB>/home", {"HOME": sb + "/home"}),
        ("HOME=<SB>/home2 XDG_CONFIG_HOME=<SB>/home2/c XDG_DATA_HOME=<SB>/home2/d "
         "XDG_STATE_HOME=<SB>/home2/s",
         {"HOME": sb + "/home2", "XDG_CONFIG_HOME": sb + "/home2/c",
          "XDG_DATA_HOME": sb + "/home2/d", "XDG_STATE_HOME": sb + "/home2/s"}),
    ]
    lines = []
    for label, extra in cases:
        out = subprocess.run([GOOSE, "info"], env=dict(base, **extra),
                             capture_output=True, text=True).stdout
        lines.append("== %s" % label)
        for l in out.splitlines():
            if any(k in l for k in ("Config dir", "Config yaml", "Sessions DB", "Logs dir")):
                lines.append("   " + " ".join(l.split()).replace(sb, "<SB>"))
    os.makedirs(outdir, exist_ok=True)
    with open(os.path.join(outdir, "info-paths.txt"), "w") as fh:
        fh.write("\n".join(lines) + "\n")
    say("info-paths written")


SCENARIOS = {
    "report": dict(args_fn=sc_report),
    "report-noquiet": dict(args_fn=sc_report_noquiet),
    "iserror": dict(args_fn=sc_iserror, provider_env={}),
    "provider-500": dict(args_fn=sc_marion_only, provider_env={"S26_FAIL500": "1"}),
    "default-mode": dict(args_fn=sc_marion_only, env_fn=env_no_mode),
    "chat-mode": dict(args_fn=sc_marion_only, env_fn=env_chat_mode),
    "builtins-default": dict(args_fn=sc_builtins_default),
    "with-builtin-dev": dict(args_fn=sc_with_builtin_dev),
    "approve-mode": dict(args_fn=sc_marion_only, env_fn=env_approve_mode),
    "config-dir-chat": dict(args_fn=sc_marion_only, env_fn=env_config_dir_chat),
    "base-path": dict(args_fn=sc_marion_only, env_fn=env_base_path),
    "host-v1": dict(args_fn=sc_marion_only, env_fn=env_host_v1),
    "session-first": dict(args_fn=sc_session_first),
    "session-resume-redeclare": dict(args_fn=sc_session_resume_redeclare),
    "session-resume": dict(args_fn=sc_session_resume),
    "session-id-resume": dict(args_fn=sc_session_id_resume),
    "isolation-xdg": dict(args_fn=sc_report, env_fn=env_isolated),
    "isolation-xdg-chat": dict(args_fn=sc_marion_only, env_fn=env_isolated_chat),
    # env-inherit: S26_MCP_LOG and S26_INHERITED only in goose's env, not in the declaration
    "env-inherit": dict(args_fn=sc_env_inherit, env_fn=env_inherit),
    # models-plain: GET /v1/models answers 200 text/plain, not JSON
    "models-plain": dict(args_fn=sc_marion_only, provider_env={"S26_MODELS_PLAIN": "1"}),
}

ORDER = ["report", "report-noquiet", "iserror", "provider-500", "default-mode", "chat-mode",
         "approve-mode", "config-dir-chat", "base-path", "host-v1",
         "builtins-default", "with-builtin-dev", "session-first", "session-resume-redeclare",
         "session-resume", "session-id-resume", "isolation-xdg", "isolation-xdg-chat",
         "env-inherit", "models-plain", "info-paths"]


def main():
    names = sys.argv[1:] or ["all"]
    if names == ["all"]:
        names = ORDER
    os.makedirs(SCRATCH, exist_ok=True)
    for n in names:
        if n == "info-paths":
            info_paths(os.path.join(SCRATCH, n))
            continue
        run_scenario(n, **SCENARIOS[n])
        if n == "session-first":
            # what the store says about the session the run just wrote
            out = subprocess.run([GOOSE, "session", "list", "--format", "json"],
                                 capture_output=True, text=True).stdout
            mine = [s for s in json.loads(out or "[]") if s.get("name") == "s26-sess"][:1]
            with open(os.path.join(SCRATCH, n, "session-list.json"), "w") as fh:
                json.dump(mine, fh, indent=2)
                fh.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
