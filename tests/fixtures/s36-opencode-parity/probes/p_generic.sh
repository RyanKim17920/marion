#!/bin/bash
# usage: NAME=<n> CFG=<json merged into file config> [INLINE=<OPENCODE_CONFIG_CONTENT>] [NOM=1 omit -m] [CALLTOOL=slow_report] p_generic.sh
cd "$(dirname "$0")"; D=$PWD/out/$NAME; P=$((39000 + RANDOM % 900))
[ -z "$CFG" ] && CFG="{}"; source mkenv.sh $D $P "$CFG"
python3 prov.py $P $D/provider.jsonl 0 ${CALLTOOL:-} & PP=$!; sleep 1
M="-m canned/canned-1"; [ -n "$NOM" ] && M=""
[ -n "$INLINE" ] && export OPENCODE_CONFIG_CONTENT="$INLINE"
(cd $SB && opencode run --pure --format json --title t $M "hi" </dev/null > $D/stdout.jsonl 2> $D/stderr.txt); echo "exit=$?" > $D/exit
kill $PP 2>/dev/null
echo "== $NAME $(cat $D/exit)"; python3 -c "
import json,sys
for l in open('$D/provider.jsonl'): r=json.loads(l); print('request model=',r['body'].get('model'),'tools=',len(r['tools']))
for l in open('$D/stdout.jsonl'):
  e=json.loads(l)
  if e['type']=='tool_use': s=e['part']['state']; print('tool_use',e['part']['tool'],s.get('status'),s.get('error'))
  if e['type']=='error': print('error',e.get('error'))
"; tail -2 $D/stderr.txt
