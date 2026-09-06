#!/usr/bin/env python3
"""Turn S27 run directories into the redacted fixture files under tests/fixtures/s27/.

    redact.py <outroot> <fixture-dir>

Substitutions (house style, after spikes/s10/redact.py and tests/fixtures/s24):
  * the run directory (with and without /private)   -> <RUN-DIR>
  * $HOME                                            -> <HOME>
  * this repo's checkout                             -> <REPO>
  * ISO timestamps / "ts" fields                     -> <TS>
  * Cline ids: 1788656391164_3a5ra (session), agent_…, conv_…, msg_…, hubreq_…
                                                     -> <SESSION-ID>, <AGENT-ID>, <CONV-ID>, …
  * the system prompt                                -> elided with its length
  * the 25 non-marion tool definitions in tools[]    -> name kept, description/parameters elided
  * the canned credential                            -> must not appear anywhere; the script
                                                        fails loudly if it does
Nothing else is touched; every remaining byte is as the CLI wrote it.
"""
import json
import os
import re
import sys

OUTROOT = os.path.abspath(sys.argv[1])
FIXDIR = os.path.abspath(sys.argv[2])
HOME = os.path.expanduser("~")
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
KEY = "marion-canned-credential-27cc"

TS = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z")
SESSION = re.compile(r"\b\d{13}_[a-z0-9]{5}\b")
# agent_1788656391190_jt66t2, conv_1788656391212_8lxjfby, hubreq_…, hub_… : prefix + epoch-ms + slug.
# Anchored on the 13-digit epoch so that event names such as `agent_start` survive untouched.
PREFIXED = re.compile(r"\b(agent|conv|hubreq|hub)_\d{13}_[a-z0-9]+\b")
UPPER = {"agent": "AGENT-ID", "conv": "CONV-ID", "hubreq": "HUBREQ-ID", "hub": "HUB-ID"}
TEAM_ID = re.compile(r"\bt_[a-z0-9]{10}\b")
TEAM_NAME = re.compile(r"\bteam-[A-Za-z0-9]{5}\b")
CLIENT_ID = re.compile(r"\bcore-[a-z0-9]{8}-[a-z0-9]{8}\b")


def redact_text(s, rundir):
    for pre in ("/private" + rundir, rundir):
        s = s.replace(pre, "<RUN-DIR>")
    for pre in ("/private" + REPO, REPO):
        s = s.replace(pre, "<REPO>")
    for pre in ("/private" + HOME, HOME):
        s = s.replace(pre, "<HOME>")
    s = TS.sub("<TS>", s)
    s = SESSION.sub("<SESSION-ID>", s)
    s = PREFIXED.sub(lambda m: "<%s>" % UPPER[m.group(1)], s)
    s = re.sub(r"\bmsg_[A-Za-z0-9_]+\b", "<MSG-ID>", s)
    s = TEAM_ID.sub("<TEAM-ID>", s)
    s = TEAM_NAME.sub("<TEAM-NAME>", s)
    s = CLIENT_ID.sub("<CLIENT-ID>", s)
    return s


def copy_stream(rundir, label):
    src = os.path.join(rundir, "stdout.jsonl")
    dst = os.path.join(FIXDIR, "cline-%s.stdout.jsonl" % label)
    with open(src) as fh:
        body = fh.read()
    with open(dst, "w") as out:
        out.write(redact_text(body, rundir))
    return dst


def copy_stderr(rundir, label):
    src = os.path.join(rundir, "stderr.txt")
    dst = os.path.join(FIXDIR, "cline-%s.stderr.txt" % label)
    with open(src) as fh:
        body = fh.read()
    with open(dst, "w") as out:
        out.write(redact_text(body, rundir))
    return dst


def copy_mcp(rundir, label):
    src = os.path.join(rundir, "mcp.jsonl")
    dst = os.path.join(FIXDIR, "cline-%s.mcp.jsonl" % label)
    with open(src) as fh:
        body = fh.read()
    with open(dst, "w") as out:
        out.write(redact_text(body, rundir))
    return dst


def copy_first_request(rundir, label):
    with open(os.path.join(rundir, "provider-requests.jsonl")) as fh:
        rec = json.loads(fh.readline())
    body = rec["body"]
    for m in body.get("messages", []):
        if m.get("role") == "system" and isinstance(m.get("content"), str):
            m["content"] = "<system prompt elided: %d chars>" % len(m["content"])
    for t in body.get("tools", []):
        fn = t.get("function") or {}
        if fn.get("name") != "marion__report":
            fn["description"] = "<elided>"
            fn["parameters"] = "<elided>"
    out = {"method": rec["method"], "path": rec["path"], "headers": rec["headers"],
           "tool_names": rec["tool_names"], "body": body}
    dst = os.path.join(FIXDIR, "cline-%s.provider-request-1.json" % label)
    with open(dst, "w") as fh:
        fh.write(redact_text(json.dumps(out, indent=2), rundir) + "\n")
    return dst


def copy_settings(rundir):
    outs = []
    for name in ("providers.json", "cline_mcp_settings.json"):
        with open(os.path.join(rundir, "data", "settings", name)) as fh:
            body = fh.read()
        body = body.replace(KEY, "<API-KEY>")
        dst = os.path.join(FIXDIR, name)
        with open(dst, "w") as out:
            out.write(redact_text(body, rundir))
        outs.append(dst)
    return outs


def copy_history(rundir, label):
    with open(os.path.join(rundir, "history.json")) as fh:
        body = fh.read()
    dst = os.path.join(FIXDIR, "cline-%s.history.json" % label)
    with open(dst, "w") as out:
        out.write(redact_text(json.dumps(json.loads(body), indent=2), rundir) + "\n")
    return dst


def copy_argv(rundir, label):
    with open(os.path.join(rundir, "meta.json")) as fh:
        meta = json.load(fh)
    out = {"argv": meta["argv"], "env_added": {k: v for k, v in meta["env"].items()
                                              if k in ("HOME", "CLINE_DIR", "CLINE_DATA_DIR", "S27_MCP_ISERROR",
                                                       "CLINE_MCP_SETTINGS_PATH", "CLINE_PROVIDER_SETTINGS_PATH")},
           "cwd": meta["cwd"], "rc": meta["rc"], "timed_out": meta["timed_out"],
           "elapsed_s": meta["elapsed_s"], "home_cline_changed": meta["home_cline_changed"],
           "cline_procs_before": meta["cline_procs_before"],
           "cline_procs_after": meta["cline_procs_after"],
           "data_tree_after": [t["path"] for t in meta["data_tree_after"]],
           "home_tree_after": [t["path"] for t in meta["home_tree_after"]]}
    dst = os.path.join(FIXDIR, "cline-%s.meta.json" % label)
    with open(dst, "w") as fh:
        fh.write(redact_text(json.dumps(out, indent=2), rundir) + "\n")
    return dst


def main():
    os.makedirs(FIXDIR, exist_ok=True)
    written = []
    ok = os.path.join(OUTROOT, "report-combo")
    written += [copy_stream(ok, "report-ok"), copy_mcp(ok, "report-ok"),
                copy_first_request(ok, "report-ok"), copy_history(ok, "report-ok"),
                copy_argv(ok, "report-ok")]
    written += copy_settings(ok)
    for label, run in (("report-iserror", "iserror"), ("provider-500", "fail500"),
                       ("report-denied-no-auto-approve", "denied")):
        d = os.path.join(OUTROOT, run)
        written.append(copy_stream(d, label))
        written.append(copy_argv(d, label))
    written.append(copy_mcp(os.path.join(OUTROOT, "iserror"), "report-iserror"))
    for label, run in (("resume-id-json", "resume-json"), ("resume-id-text", "resume-text")):
        d = os.path.join(OUTROOT, run)
        written.append(copy_stderr(d, label))
        written.append(copy_argv(d, label))
    # the daemon evidence: the first-ever run (no --data-dir flag) that spawned a hub daemon
    probe = os.path.join(OUTROOT, "probe1")
    written.append(copy_argv(probe, "env-only-spawns-hub-daemon"))
    with open(os.path.join(probe, "data", "locks", "hub", "production.json")) as fh:
        lock = json.load(fh)
    lock["authToken"] = "<HUB-AUTH-TOKEN>"
    dst = os.path.join(FIXDIR, "cline-env-only-spawns-hub-daemon.lock.json")
    with open(dst, "w") as fh:
        fh.write(redact_text(json.dumps(lock, indent=2), probe) + "\n")
    written.append(dst)
    flagonly = os.path.join(OUTROOT, "report-datadir")
    written.append(copy_argv(flagonly, "flags-only-leaks-home"))
    # settings documents outside the data dir, pointed at by env
    em = os.path.join(OUTROOT, "ext-mcp")
    written += [copy_stream(em, "mcp-settings-path-env"), copy_mcp(em, "mcp-settings-path-env"),
                copy_argv(em, "mcp-settings-path-env")]
    ep = os.path.join(OUTROOT, "ext-providers")
    written += [copy_stream(ep, "provider-settings-path-env-ignored"),
                copy_stderr(ep, "provider-settings-path-env-ignored"),
                copy_argv(ep, "provider-settings-path-env-ignored")]
    with open(os.path.join(ep, "data", "settings", "providers.json")) as fh:
        body = fh.read()
    dst = os.path.join(FIXDIR, "cline-provider-settings-path-env-ignored.default-providers.json")
    with open(dst, "w") as fh:
        fh.write(redact_text(body, ep))
    written.append(dst)
    hits = []
    with open(os.path.join(ep, "data", "logs", "cline.log")) as fh:
        for line in fh:
            if "api.cline.bot/api/v1/chat/completions" in line:
                rec = json.loads(line)
                err = rec.get("err") or {}
                hits.append({"time": rec.get("time"), "msg": rec.get("msg"),
                             "providerId": rec.get("providerId"), "error": err.get("name"),
                             "message": err.get("message"), "url": err.get("url"),
                             "requested_model": (err.get("requestBodyValues") or {}).get("model")})
    dst = os.path.join(FIXDIR, "cline-provider-settings-path-env-ignored.vendor-requests.json")
    with open(dst, "w") as fh:
        fh.write(redact_text(json.dumps(hits, indent=2), ep) + "\n")
    written.append(dst)

    # -m on argv versus the model in providers.json
    mf = os.path.join(OUTROOT, "model-flag")
    written += [copy_stream(mf, "model-flag-wins"), copy_first_request(mf, "model-flag-wins"),
                copy_argv(mf, "model-flag-wins")]
    with open(os.path.join(mf, "data", "settings", "providers.json")) as fh:
        body = fh.read().replace(KEY, "<API-KEY>")
    dst = os.path.join(FIXDIR, "cline-model-flag-wins.rewritten-providers.json")
    with open(dst, "w") as fh:
        fh.write(redact_text(body, mf))
    written.append(dst)

    bad = [p for p in written if KEY in open(p).read()]
    if bad:
        sys.exit("credential leaked into: %s" % bad)
    for p in written:
        print(os.path.relpath(p, FIXDIR), os.path.getsize(p))


if __name__ == "__main__":
    main()
