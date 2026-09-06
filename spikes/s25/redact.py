#!/usr/bin/env python3
"""Turn S25 run directories into the redacted fixture files under tests/fixtures/s25/.

    ./redact.py <runs-root> <fixture-dir>

Substitutions (house style, S10 numbering):
  * the runs root                 -> <RUN-DIR>        (with and without macOS /private, and slugged)
  * the repository checkout       -> <REPO>
  * the python interpreter path   -> <PYTHON>
  * $HOME                         -> <HOME>
  * UUIDs                         -> <UUID-N>, numbered by first appearance per file
  * the canned key                -> <KEY>            (belt and braces: it must not be there anyway)
  * `slash_commands` / `agents` arrays in the init frame -> a count marker (they list the
    operator's own skills, which qwen discovers even under QWEN_HOME — see README item 12)
  * provider requests: the system prompt -> its length; every `<system-reminder>` text part that
    is not the MCP-tools reminder -> its length; tool descriptions/parameters -> kept for the
    tools named in `tools[]` (that list is the finding), except long built-in descriptions are
    cut to their length when the request is a baseline with more than ten tools.
Nothing else is touched.
"""
import json
import os
import re
import sys

runs_root, fixdir = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
HOME = os.path.expanduser("~")
KEY = "marion-canned-credential-25aa"
PYTHON = sys.executable

UUID = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")


class Redactor:
    def __init__(self):
        self.uuids = {}

    def text(self, s):
        for pre in ("/private" + runs_root, runs_root):
            s = s.replace(pre, "<RUN-DIR>")
            s = s.replace(pre.replace("/", "-"), "<RUN-DIR-SLUG>")
        s = s.replace(PYTHON, "<PYTHON>")
        for pre in ("/private" + REPO, REPO):
            s = s.replace(pre, "<REPO>")
        for pre in ("/private" + HOME, HOME):
            s = s.replace(pre, "<HOME>")
        s = s.replace(KEY, "<KEY>")
        return UUID.sub(lambda m: self.uuids.setdefault(m.group(0), "<UUID-%d>" % (len(self.uuids) + 1)), s)


def elide(s):
    return "<elided: %d chars>" % len(s)


def redact_stream(src, dst):
    r = Redactor()
    with open(src) as fh, open(dst, "w") as out:
        for line in fh:
            if not line.strip():
                continue
            try:
                frame = json.loads(line)
            except ValueError:
                out.write(r.text(line))
                continue
            if frame.get("type") == "system" and frame.get("subtype") == "init":
                for k in ("slash_commands", "agents"):
                    if isinstance(frame.get(k), list):
                        frame[k] = ["<elided: %d entries>" % len(frame[k])]
            out.write(r.text(json.dumps(frame, ensure_ascii=False)) + "\n")


def redact_request(body):
    for m in body.get("messages", []) or []:
        c = m.get("content")
        if m.get("role") == "system" and isinstance(c, str):
            m["content"] = elide(c)
        elif isinstance(c, list):
            for part in c:
                t = part.get("text") if isinstance(part, dict) else None
                if isinstance(t, str) and t.startswith("<system-reminder>") and "mcp__" not in t:
                    part["text"] = elide(t)
    tools = body.get("tools", []) or []
    if len(tools) > 10:
        for t in tools:
            fn = t.get("function") or {}
            if fn.get("name", "").startswith("mcp__"):
                continue
            if isinstance(fn.get("description"), str):
                fn["description"] = elide(fn["description"])
            if "parameters" in fn:
                fn["parameters"] = "<elided: %d chars>" % len(json.dumps(fn["parameters"]))
    return body


def provider_request(run, seq, dst):
    r = Redactor()
    with open(os.path.join(runs_root, run, "provider.jsonl")) as fh:
        for line in fh:
            rec = json.loads(line)
            if rec.get("seq") == seq:
                keep = {"seq": rec["seq"], "path": rec["path"], "method": rec["method"],
                        "kind": rec["kind"], "headers": rec["headers"], "tool_names": rec["tool_names"],
                        "body": redact_request(rec["body"])}
                with open(dst, "w") as out:
                    out.write(r.text(json.dumps(keep, indent=2, ensure_ascii=False)) + "\n")
                return
    raise SystemExit("no request %d in %s" % (seq, run))


def provider_timeline(run, dst):
    """Every request as seq/offset/kind only — the retry cadence is the finding, not 28 bodies."""
    recs = [json.loads(l) for l in open(os.path.join(runs_root, run, "provider.jsonl"))]
    t0 = recs[0]["t"]
    with open(dst, "w") as out:
        for rec in recs:
            out.write(json.dumps({"seq": rec["seq"], "t_offset_s": round(rec["t"] - t0, 1),
                                  "path": rec["path"], "kind": rec["kind"],
                                  "tool_count": len(rec["tool_names"])}) + "\n")


def copy_text(src, dst):
    r = Redactor()
    with open(src) as fh, open(dst, "w") as out:
        out.write(r.text(fh.read()))


def mcp(run, dst):
    copy_text(os.path.join(runs_root, run, "mcp.jsonl"), dst)


os.makedirs(fixdir, exist_ok=True)
for stale in os.listdir(fixdir):
    if stale != "README.md":
        os.remove(os.path.join(fixdir, stale))
F = lambda name: os.path.join(fixdir, name)
R = lambda run, *parts: os.path.join(runs_root, run, *parts)

# The run the adapter compiles.
redact_stream(R("core-plus-exclude", "stdout.jsonl"), F("qwen-write-then-report.stdout.jsonl"))
provider_request("core-plus-exclude", 1, F("qwen-write-then-report.provider-request-1.json"))
mcp("core-plus-exclude", F("qwen-write-then-report.mcp.jsonl"))
copy_text(R("core-plus-exclude", "home", "settings.json"), F("qwen-write-then-report.settings.json"))
copy_text(R("core-plus-exclude", "argv.json"), F("qwen-write-then-report.argv.json"))
copy_text(R("core-plus-exclude", "env.json"), F("qwen-write-then-report.process-env.json"))
copy_text(R("core-plus-exclude", "stderr.txt"), F("qwen-write-then-report.stderr.txt"))

# Tool-list axes.
provider_request("baseline-yolo", 1, F("qwen-baseline-deferred-mcp.provider-request-1.json"))
redact_stream(R("baseline-yolo", "stdout.jsonl"), F("qwen-baseline-deferred-mcp.stdout.jsonl"))
provider_request("blocking-mcp", 1, F("qwen-blocking-mcp.provider-request-1.json"))
provider_request("visible-tools", 1, F("qwen-visible-tools.provider-request-1.json"))
copy_text(R("visible-tools", "home", "settings.json"), F("qwen-visible-tools.settings.json"))
provider_request("write-then-report", 1, F("qwen-core-tools-alone.provider-request-1.json"))
redact_stream(R("bare-mcp-config", "stdout.jsonl"), F("qwen-bare-mcp-config.stdout.jsonl"))
redact_stream(R("mcp-config-argv", "stdout.jsonl"), F("qwen-mcp-config-argv.stdout.jsonl"))
provider_request("mcp-config-argv", 1, F("qwen-mcp-config-argv.provider-request-1.json"))
mcp("mcp-config-argv", F("qwen-mcp-config-argv.mcp.jsonl"))
copy_text(R("mcp-config-argv", "argv.json"), F("qwen-mcp-config-argv.argv.json"))
copy_text(R("mcp-config-argv", "home-after.txt"), F("qwen-mcp-config-argv.home-after.txt"))
copy_text(R("mcp-config-argv", "home", "settings.json"), F("qwen-mcp-config-argv.settings.json"))
redact_stream(R("core-tools-empty", "stdout.jsonl"), F("qwen-core-tools-empty.stdout.jsonl"))
provider_request("core-tools-empty", 1, F("qwen-core-tools-empty.provider-request-1.json"))
copy_text(R("core-tools-empty", "argv.json"), F("qwen-core-tools-empty.argv.json"))
copy_text(R("core-tools-empty", "stderr.txt"), F("qwen-core-tools-empty.stderr.txt"))
redact_stream(R("relative-write", "stdout.jsonl"), F("qwen-relative-write.stdout.jsonl"))
copy_text(R("relative-write", "work-after.txt"), F("qwen-relative-write.work-after.txt"))

# Failure shapes.
redact_stream(R("report-iserror", "stdout.jsonl"), F("qwen-report-iserror.stdout.jsonl"))
redact_stream(R("denied-without-yolo", "stdout.jsonl"), F("qwen-denied-without-yolo.stdout.jsonl"))
provider_request("denied-without-yolo", 2, F("qwen-denied-without-yolo.provider-request-2.json"))
redact_stream(R("provider-500", "stdout.jsonl"), F("qwen-provider-500.stdout.jsonl"))
provider_timeline("provider-500", F("qwen-provider-500.provider-timeline.jsonl"))
redact_stream(R("provider-500-wall-time", "stdout.jsonl"), F("qwen-provider-500-wall-time.stdout.jsonl"))
copy_text(R("provider-500-wall-time", "stderr.txt"), F("qwen-provider-500-wall-time.stderr.txt"))
redact_stream(R("probe-no-openai-env", "stdout.jsonl"), F("qwen-no-auth.stdout.jsonl"))

# Auth, isolation, side turns, project settings, resume.
copy_text(R("probe-settings-auth", "home", "settings.json"), F("qwen-settings-auth.settings.json"))
redact_stream(R("probe-settings-auth", "stdout.jsonl"), F("qwen-settings-auth.stdout.jsonl"))
copy_text(R("probe-env-only", "home-after.txt"), F("qwen-env-only.home-after.txt"))
provider_request("probe-env-only", 2, F("qwen-memory-side-turn.provider-request-2.json"))
redact_stream(R("project-settings", "stdout.jsonl"), F("qwen-project-settings.stdout.jsonl"))
copy_text(R("project-settings", "work-after.txt"), F("qwen-project-settings.work-after.txt"))
redact_stream(R("resume-turn-1", "stdout.jsonl"), F("qwen-resume-turn-1.stdout.jsonl"))
redact_stream(R("resume-turn-2", "stdout.jsonl"), F("qwen-resume-turn-2.stdout.jsonl"))
provider_request("resume-turn-2", 1, F("qwen-resume-turn-2.provider-request-1.json"))
copy_text(R("resume-turn-2", "argv.json"), F("qwen-resume-turn-2.argv.json"))

for name in sorted(os.listdir(fixdir)):
    p = os.path.join(fixdir, name)
    # README.md names the key on purpose (as s24's does), so it is the one file allowed to.
    if name != "README.md" and KEY in open(p, errors="replace").read():
        raise SystemExit("KEY leaked into %s" % name)
    print("%-60s %6d" % (name, os.path.getsize(p)))
