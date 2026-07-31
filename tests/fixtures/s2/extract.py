#!/usr/bin/env python3
"""Split .rec into raw output .bin plus an asciicast v3 file."""
import json, os, struct, sys

S = os.path.dirname(os.path.abspath(__file__))

def load(p):
    buf = open(p, "rb").read(); i = 0
    while i + 13 <= len(buf):
        tag = buf[i:i+1]; ts, n = struct.unpack("<dI", buf[i+1:i+13])
        yield tag, ts, buf[i+13:i+13+n]
        i += 13 + n

for name in sys.argv[1:]:
    p = f"{S}/{name}.rec"
    if not os.path.exists(p):
        continue
    recs = list(load(p))
    raw = b"".join(pl for t, _, pl in recs if t == b"O")
    open(f"{S}/{name}.raw.bin", "wb").write(raw)
    # asciicast v3: header line then [interval, code, data] with RELATIVE times
    cast = [json.dumps({"version": 3, "term": {"cols": 120, "rows": 40, "type": "xterm-256color"},
                        "timestamp": 0, "env": {"TERM": "xterm-256color"}})]
    prev = 0.0
    for t, ts, pl in recs:
        code = {b"O": "o", b"I": "i", b"R": "r"}[t]
        if code == "r":
            r, c = json.loads(pl); data = f"{c}x{r}"
        else:
            data = pl.decode("utf-8", "replace")
        cast.append(json.dumps([round(ts - prev, 6), code, data]))
        prev = ts
    open(f"{S}/{name}.cast", "w").write("\n".join(cast) + "\n")
    print(name, "raw", len(raw), "cast_events", len(recs))
