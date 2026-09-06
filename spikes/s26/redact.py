#!/usr/bin/env python3
"""Turn S26 run directories into the redacted fixture files.

    ./redact.py <runs-dir> <fixture-dir>

Substitutions (house style, S10/S24 numbering):
  * each run directory <runs-dir>/<scenario>   -> <RUN-DIR>        (with and without macOS /private)
  * the repository root                          -> <REPO>
  * $HOME                                        -> <HOME>
  * UUIDs                                        -> <UUID>
  * `"created": <unix seconds>` in goose frames  -> `"created": "<TS>"` (kept a JSON value)
  * `<current-time>…</current-time>` in prompts  -> `<current-time><TS></current-time>`
  * 127.0.0.1:<port>                             -> 127.0.0.1:<PORT>
  * provider request `t` (absolute seconds)      -> `t_rel` (seconds since that log's first record)
  * every `role: "system"` message body          -> `<system prompt elided: N chars>`
Nothing else is touched; every remaining byte is as goose (or the stubs) wrote it.

The canned key `marion-canned-credential-26bb` is asserted absent from every file written.
"""
import json
import os
import re
import sys

runs, fixdir = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
HOME = os.path.expanduser("~")
KEY = "marion-canned-credential-26bb"

# no leading \b: goose message ids are `msg_<uuid>` and `_` is a word character
UUID = re.compile(r"(?<![0-9a-f])[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
CREATED = re.compile(r'"created":\s*\d{10}')
CURTIME = re.compile(r"<current-time>[^<]*</current-time>")
PORT = re.compile(r"127\.0\.0\.1:\d+")

written = []


def redact_text(s, rundir):
    for pre in ("/private" + rundir, rundir):
        s = s.replace(pre, "<RUN-DIR>")
    for pre in ("/private" + runs, runs):
        s = s.replace(pre, "<RUNS>")
    for pre in ("/private" + REPO, REPO):
        s = s.replace(pre, "<REPO>")
    for pre in ("/private" + HOME, HOME):
        s = s.replace(pre, "<HOME>")
    s = UUID.sub("<UUID>", s)
    s = CREATED.sub('"created":"<TS>"', s)
    s = CURTIME.sub("<current-time><TS></current-time>", s)
    s = PORT.sub("127.0.0.1:<PORT>", s)
    return s


def elide_system(body):
    for m in body.get("messages", []) or []:
        if m.get("role") == "system" and isinstance(m.get("content"), str):
            m["content"] = "<system prompt elided: %d chars>" % len(m["content"])
    return body


def out(name, text):
    assert KEY not in text, "canned key leaked into %s" % name
    os.makedirs(fixdir, exist_ok=True)
    p = os.path.join(fixdir, name)
    with open(p, "w") as fh:
        fh.write(text if text.endswith("\n") else text + "\n")
    written.append(name)


def copy(scn, src, dst):
    rundir = os.path.join(runs, scn)
    p = os.path.join(rundir, src)
    if not os.path.exists(p):
        print("missing", p)
        return
    out(dst, redact_text(open(p).read(), rundir))


def provider_log(scn, dst):
    """Whole request log, system prompts elided, `t` made relative."""
    rundir = os.path.join(runs, scn)
    recs = [json.loads(l) for l in open(os.path.join(rundir, "provider-requests.jsonl"))]
    t0 = recs[0]["t"] if recs else 0
    lines = []
    for r in recs:
        r = dict(r)
        r["t_rel"] = round(r.pop("t") - t0, 2)
        if "body" in r:
            r["body"] = elide_system(r["body"])
        lines.append(json.dumps(r))
    out(dst, redact_text("\n".join(lines), rundir))


def provider_request(scn, dst, pick):
    """One request body (pretty-printed), system prompt elided. `pick(rec)` selects it."""
    rundir = os.path.join(runs, scn)
    for l in open(os.path.join(rundir, "provider-requests.jsonl")):
        r = json.loads(l)
        if r["method"] == "POST" and pick(r):
            doc = {"path": r["path"], "headers": r["headers"], "body": elide_system(r["body"])}
            out(dst, redact_text(json.dumps(doc, indent=2), rundir))
            return
    print("no matching request in", scn)


def real_turn(r):
    return r["kind"] == "turn-with-marion-tool"


def meta(scn):
    return json.load(open(os.path.join(runs, scn, "meta.json")))


# --- the run the adapter compiles -------------------------------------------------------------
copy("report", "stdout.jsonl", "goose-report.stdout.jsonl")
copy("report", "mcp.jsonl", "goose-report.mcp.jsonl")
provider_log("report", "goose-report.provider-requests.jsonl")
provider_request("report", "goose-report.provider-request-1.json", real_turn)
m = meta("report")
out("goose-report.meta.json", redact_text(json.dumps(
    {k: m[k] for k in ("goose_version", "argv", "env", "cwd", "exit_code", "duration_s")},
    indent=2), os.path.join(runs, "report")))
copy("report-noquiet", "stdout.jsonl", "goose-report-noquiet.stdout.txt")

# --- failure shapes ---------------------------------------------------------------------------
copy("iserror", "stdout.jsonl", "goose-report-iserror.stdout.jsonl")
copy("iserror", "mcp.jsonl", "goose-report-iserror.mcp.jsonl")
copy("provider-500", "stdout.jsonl", "goose-provider-500.stdout.jsonl")
provider_log("provider-500", "goose-provider-500.provider-requests.jsonl")

# --- approval modes ---------------------------------------------------------------------------
copy("default-mode", "stdout.jsonl", "goose-default-mode.stdout.jsonl")
copy("chat-mode", "stdout.jsonl", "goose-chat-mode.stdout.jsonl")
copy("approve-mode", "stdout.jsonl", "goose-approve-mode.stdout.jsonl")
copy("approve-mode", "stderr.txt", "goose-approve-mode.stderr.txt")

# --- what the model is offered ----------------------------------------------------------------
provider_request("builtins-default", "goose-builtins-default.provider-request-1.json", real_turn)
provider_request("with-builtin-dev", "goose-with-builtin-developer.provider-request-1.json",
                 real_turn)

# --- provider URL composition -----------------------------------------------------------------
lines = []
for scn, label in (("iserror", "OPENAI_HOST=http://127.0.0.1:<PORT>  (OPENAI_BASE_PATH unset)"),
                   ("base-path", "OPENAI_HOST=http://127.0.0.1:<PORT>  OPENAI_BASE_PATH=custom/chat/completions"),
                   ("host-v1", "OPENAI_HOST=http://127.0.0.1:<PORT>/v1  (OPENAI_BASE_PATH unset)")):
    lines.append("== " + label)
    for l in open(os.path.join(runs, scn, "provider-requests.jsonl")):
        r = json.loads(l)
        lines.append("   %s %s  (%s)" % (r["method"], r["path"], r["kind"]))
out("goose-provider-paths.txt", "\n".join(lines))

# --- sessions and resume ----------------------------------------------------------------------
copy("session-first", "stdout.jsonl", "goose-session-first.stdout.jsonl")
copy("session-first", "mcp.jsonl", "goose-session-first.mcp.jsonl")
copy("session-first", "session-list.json", "goose-session-list.json")
copy("session-resume-redeclare", "stdout.jsonl", "goose-session-resume-redeclare.stdout.txt")
copy("session-resume", "stdout.jsonl", "goose-session-resume.stdout.jsonl")
copy("session-resume", "stderr.txt", "goose-session-resume.stderr.txt")
provider_request("session-resume", "goose-session-resume.provider-request-1.json",
                 lambda r: True)
copy("session-id-resume", "stdout.jsonl", "goose-session-id-resume.stdout.jsonl")
out("goose-session-resume.meta.json", redact_text(json.dumps(
    {s: {k: meta(s)[k] for k in ("argv", "exit_code")}
     for s in ("session-first", "session-resume-redeclare", "session-resume", "session-id-resume")},
    indent=2), "/nonexistent-rundir"))

# --- isolation --------------------------------------------------------------------------------
copy("config-dir-chat", "stdout.jsonl", "goose-config-dir-chat.stdout.jsonl")
copy("isolation-xdg-chat", "stdout.jsonl", "goose-xdg-config-chat.stdout.jsonl")
state = {}
for scn in ("report", "isolation-xdg", "isolation-xdg-chat"):
    mm = meta(scn)
    reloc = {k: v for k, v in mm["env"].items()
             if k in ("HOME", "GOOSE_CONFIG_DIR", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
                      "XDG_STATE_HOME", "XDG_CACHE_HOME")}
    sd = mm["state_dirs"]
    own = os.path.join(runs, scn)
    state[scn] = {
        "relocation_env": reloc,
        "created": [p for p in sd["created"] if not p.startswith(("/private" + own, own))
                    or "/sandbox/" in p],
        "modified": [p for p in sd["modified"] if not p.startswith(("/private" + own, own))
                     or "/sandbox/" in p],
    }
out("goose-state-dirs.json", redact_text(json.dumps(state, indent=2), "/nonexistent-rundir"))
copy("info-paths", "info-paths.txt", "goose-info-paths.txt")

# --- follow-ups: env inheritance by the extension child, non-JSON models probe -----------------
for src, dst in (("mcp.jsonl", "goose-env-inherit.mcp.jsonl"),
                 ("mcp-argv.jsonl", "goose-env-inherit.mcp-argv.jsonl"),
                 ("stdout.jsonl", "goose-env-inherit.stdout.jsonl")):
    if os.path.exists(os.path.join(runs, "env-inherit", src)):
        copy("env-inherit", src, dst)
m = meta("env-inherit")
out("goose-env-inherit.meta.json", redact_text(json.dumps(
    {k: m[k] for k in ("argv", "env", "exit_code")}, indent=2), os.path.join(runs, "env-inherit")))
copy("models-plain", "stdout.jsonl", "goose-models-plain.stdout.jsonl")
copy("models-plain", "stderr.txt", "goose-models-plain.stderr.txt")
provider_log("models-plain", "goose-models-plain.provider-requests.jsonl")
out("goose-models-plain.meta.json", redact_text(json.dumps(
    {k: meta("models-plain")[k] for k in ("exit_code", "duration_s")}, indent=2), runs))

# --- final check ------------------------------------------------------------------------------
for n in written:
    assert KEY not in open(os.path.join(fixdir, n)).read(), n
print("%d files written to %s; canned key present in none" % (len(written), fixdir))
