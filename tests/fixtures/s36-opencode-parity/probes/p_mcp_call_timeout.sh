#!/bin/bash
# Probe: does `opencode run` give up on an MCP tools/call that takes CALL seconds (marion's blocking spawn/wait)?
cd "$(dirname "$0")"; D=$PWD/out/mcp-call-${CALL}s-t${TMO:-default}; P=38923
X="{\"mcp\":{\"slow\":{\"environment\":{\"SLOW_DELAY\":\"0\",\"SLOW_CALL\":\"$CALL\"}}}}"
[ -n "$TMO" ] && X="{\"mcp\":{\"slow\":{\"environment\":{\"SLOW_DELAY\":\"0\",\"SLOW_CALL\":\"$CALL\"},\"timeout\":$TMO}}}"
source mkenv.sh $D $P "$X"
python3 prov.py $P $D/provider.jsonl 0 slow_report & PP=$!; sleep 1
(cd $SB && opencode run --pure --format json --title t -m canned/canned-1 "hi" </dev/null > $D/stdout.jsonl 2> $D/stderr.txt); echo "exit=$?" > $D/exit
kill $PP 2>/dev/null
python3 - $D <<'PY'
import json, sys
d = sys.argv[1]
tu = [json.loads(l) for l in open(f"{d}/stdout.jsonl") if '"tool_use"' in l]
st = tu[0]["part"]["state"] if tu else {}
t = st.get("time", {})
dur = (t.get("end", 0) - t.get("start", 0)) / 1000
line = f"tool_use status={st.get('status')} after {dur:.1f}s error={st.get('error')!r} output={str(st.get('output'))[:60]!r}"
v = "PASS: the long call completed" if st.get("status") == "completed" else "FAIL: the long call was abandoned"
open(f"{d}/verdict.txt", "w").write(line + "\n" + v + "\n"); print(line); print(v)
PY
