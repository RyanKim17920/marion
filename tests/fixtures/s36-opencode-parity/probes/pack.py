# pack.py <fixture-dir> <name>=<probe-out-dir> ... : copy a probe's transcript into a fixture, redacted.
import json, os, re, sys
dest = sys.argv[1]
SCR = os.path.dirname(os.path.abspath(__file__))
sess = {}
def red(s):
    s = s.replace(SCR, "<SCRATCH>")
    s = re.sub(r"/Users/[^/\"]+", "<HOME>", s)
    def fake(m):
        sid = m.group(0)
        if sid not in sess: sess[sid] = "ses_s36fake" + str(len(sess) + 2).rjust(19, "0")
        return sess[sid]
    return re.sub(r"ses_[A-Za-z0-9]{26}", fake, s)
for arg in sys.argv[2:]:
    name, src = arg.split("=", 1)
    out = os.path.join(dest, name); os.makedirs(out, exist_ok=True)
    for f in ["stdout.jsonl", "mcp.jsonl", "acp.jsonl", "verdict.txt", "exit"]:
        p = os.path.join(src, f)
        if os.path.exists(p):
            tgt = {"mcp.jsonl": "mcp-server.jsonl", "acp.jsonl": "acp-client.jsonl"}.get(f, f)
            open(os.path.join(out, tgt), "w").write(red(open(p).read()))
    p = os.path.join(src, "provider.jsonl")
    if os.path.exists(p):
        with open(os.path.join(out, "provider-requests.summary.jsonl"), "w") as o:
            for l in open(p):
                r = json.loads(l); b = r["body"]
                o.write(red(json.dumps({"t": r["t"], "path": r["path"], "model": b.get("model"), "tools": r["tools"],
                    "stream_options": b.get("stream_options"), "roles": [m.get("role") for m in b.get("messages", [])]})) + "\n")
