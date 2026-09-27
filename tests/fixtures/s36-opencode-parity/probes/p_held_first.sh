#!/bin/bash
# Probe: while opencode run's first model request is held, has it printed any frame (its session id)?
cd "$(dirname "$0")"; D=$PWD/out/held-first; P=39911
source mkenv.sh $D $P '{}'
python3 prov.py $P $D/provider.jsonl 20 & PP=$!; sleep 1
(cd $SB && opencode run --pure --format json --title t -m canned/canned-1 "hi" </dev/null > $D/stdout.jsonl 2>$D/stderr.txt) & OC=$!
for i in $(seq 1 60); do sleep 1; [ -s $D/provider.jsonl ] && break; done
sleep 3
{ echo "first request held for 20 s; frames on stdout 3 s into the hold: $(wc -l < $D/stdout.jsonl)"; } > $D/verdict.txt
wait $OC
echo "frames after the run: $(wc -l < $D/stdout.jsonl)" >> $D/verdict.txt
if [ "$(sed -n 1p $D/verdict.txt | grep -o '[0-9]*$')" = "0" ]; then echo "FAIL: no frame, so no session id, until the first response streams" >> $D/verdict.txt; else echo "PASS: the session id is on stdout before the first response" >> $D/verdict.txt; fi
kill $PP
cat $D/verdict.txt
