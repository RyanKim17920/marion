#!/bin/bash
# usage: NAME=<n> SLOWENV='{"SLOW_DELAY":"8"}' [CALLTOOL=slow_report] p_acp.sh
cd "$(dirname "$0")"; D=$PWD/out/$NAME; P=$((39000 + RANDOM % 900))
[ -z "$ACPCFG" ] && ACPCFG="{}"; source mkenv.sh $D $P "$ACPCFG"
python3 - $SB/config/opencode/opencode.json <<'PY'
import json,sys; c=json.load(open(sys.argv[1])); c.pop("mcp",None); json.dump(c,open(sys.argv[1],"w"))
PY
python3 prov.py $P $D/provider.jsonl 0 ${CALLTOOL:-} & PP=$!; sleep 1
SE=$(python3 -c "import json,sys; e=json.loads(sys.argv[1]); e['SLOW_LOG']='$D/mcp.jsonl'; print(json.dumps(e))" "${SLOWENV:-{\}}")
SLOWMCP=$PWD/slowmcp.py timeout 900 python3 drive_acp.py $D/acp.jsonl "$SE" $SB; echo "exit=$?" > $D/exit
kill $PP 2>/dev/null
python3 - $D <<'PY'
import json, sys
d = sys.argv[1]
acp = [json.loads(l) for l in open(f"{d}/acp.jsonl")]
mcp = [json.loads(l) for l in open(f"{d}/mcp.jsonl")] if __import__("os").path.exists(f"{d}/mcp.jsonl") else []
req = [json.loads(l) for l in open(f"{d}/provider.jsonl")] if __import__("os").path.exists(f"{d}/provider.jsonl") else []
t0 = min([e["t"] for e in mcp] + [r["t"] for r in req]) if (mcp or req) else 0
for e in acp:
    m = e["m"]; meth = m.get("method") or ("result" if "result" in m else "error" if "error" in m else "?")
    extra = ""
    if meth == "session/update":
        u = m["params"]["update"]; extra = u.get("sessionUpdate") + " " + str(u.get("status", "")) + " " + str(u.get("title", ""))[:40]
        if u.get("rawOutput"): extra += " out=" + json.dumps(u["rawOutput"])[:120]
    elif "result" in m: extra = json.dumps(m["result"])[:100]
    elif "error" in m: extra = json.dumps(m["error"])[:160]
    print(f"{e['t']:6.1f} {e['dir']} {meth} {extra}")
init = next((x["t"] for x in mcp if x["ev"] == "sent" and x["method"] == "initialize"), None)
print("mcp initialize answered at", None if init is None else round(init - t0, 1), "; provider requests:", [(round(r["t"] - t0, 1), "slow_report" in r["tools"]) for r in req])
PY
