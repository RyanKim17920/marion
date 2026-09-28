#!/bin/bash
# Probe: does `opencode run` hold its first model request until a slow MCP server answers initialize?
cd "$(dirname "$0")"; D=$PWD/out/mcp-ready-run; P=38921
source mkenv.sh $D $P
python3 prov.py $P $D/provider.jsonl & PP=$!; sleep 1
T0=$(python3 -c 'import time;print(time.time())')
(cd $SB && opencode run --pure --format json --title t -m canned/canned-1 "hi" </dev/null > $D/stdout.jsonl 2> $D/stderr.txt); echo "exit=$?" > $D/exit
kill $PP
python3 - $D $T0 <<'PY'
import json, sys
d, t0 = sys.argv[1], float(sys.argv[2])
mcp = [json.loads(l) for l in open(f"{d}/mcp.jsonl")]
req = [json.loads(l) for l in open(f"{d}/provider.jsonl")]
init_sent = next((e["t"] for e in mcp if e["ev"] == "sent" and e["method"] == "initialize"), None)
first = req[0] if req else None
lines = [f"mcp start +{mcp[0]['t']-t0:.1f}s, initialize answered +{(init_sent or 0)-t0:.1f}s",
         f"first model request +{first['t']-t0:.1f}s tools={first['tools']}" if first else "no model request"]
ok = first and init_sent and first["t"] >= init_sent and "slow_report" in first["tools"]
lines.append("PASS: first request waited for the MCP server and carried its tool" if ok else "FAIL: first request went before the MCP server was ready or without its tool")
open(f"{d}/verdict.txt", "w").write("\n".join(lines) + "\n"); print("\n".join(lines))
PY
