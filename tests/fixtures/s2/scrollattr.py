#!/usr/bin/env python3
"""Attribute every scroll event to the DECSTBM region active at the time.

Minimal VT cursor/region state machine. Only tracks what matters:
  - DECSTBM (CSI t;b r)  -> region
  - CUP (CSI r;c H / f), CUU/CUD (A/B), VPA (d), CR/LF
  - LF / IND (ESC D) at region bottom -> SCROLL UP  (feeds terminal scrollback)
  - RI (ESC M) at region top          -> SCROLL DOWN (never feeds scrollback)
  - CSI n S (SU) / CSI n T (SD)
  - DECSTBM also homes the cursor per spec.
Reports, per stream: counts of scroll-up and scroll-down keyed by (region_top, region_bottom).
"""
import collections, json, re, sys, os

CSI_FINAL = set("@ABCDEFGHIJKLMNPQRSTXZ`abcdefghilmnpqrstuvwxyz{|}~")

def replay(events, rows0, cols0):
    rows = rows0
    top, bot = 1, rows          # 1-based inclusive DECSTBM region
    cy = 1
    up = collections.Counter()   # (top,bot,rows) -> lines scrolled up
    down = collections.Counter()
    up_events = collections.Counter()

    def lf(n=1):
        nonlocal cy
        for _ in range(n):
            if cy == bot:
                up[(top, bot, rows)] += 1
                up_events[(top, bot, rows)] += 0  # placeholder
            elif cy < rows:
                cy += 1

    def ri(n=1):
        nonlocal cy
        for _ in range(n):
            if cy == top:
                down[(top, bot, rows)] += 1
            elif cy > 1:
                cy -= 1

    for kind, data in events:
        if kind == "r":
            c, r = data
            rows = r
            top, bot = 1, rows
            cy = min(cy, rows)
            continue
        b = data
        i = 0
        n = len(b)
        while i < n:
            ch = b[i]
            if ch == 0x1b:
                if i + 1 < n and b[i+1] == 0x5b:  # CSI
                    j = i + 2
                    while j < n and chr(b[j]) not in CSI_FINAL:
                        j += 1
                    if j >= n:
                        i = n; break
                    params = b[i+2:j].decode("latin1")
                    final = chr(b[j])
                    nums = [p for p in re.split(r"[;:]", params.lstrip("?><!")) ]
                    def num(k, d):
                        try:
                            v = nums[k]
                            return int(v) if v else d
                        except (IndexError, ValueError):
                            return d
                    priv = params[:1] in ("?", ">", "<", "!")
                    if not priv:
                        if final == "r":
                            top = num(0, 1); bot = num(1, rows)
                            if top < 1: top = 1
                            if bot > rows: bot = rows
                            if top >= bot: top, bot = 1, rows
                            cy = 1
                        elif final in "Hf":
                            cy = max(1, min(rows, num(0, 1)))
                        elif final == "d":
                            cy = max(1, min(rows, num(0, 1)))
                        elif final == "A":
                            cy = max(1, cy - num(0, 1))
                        elif final == "B":
                            cy = min(rows, cy + num(0, 1))
                        elif final == "E":
                            cy = min(rows, cy + num(0, 1))
                        elif final == "F":
                            cy = max(1, cy - num(0, 1))
                        elif final == "S":
                            up[(top, bot, rows)] += num(0, 1)
                        elif final == "T":
                            down[(top, bot, rows)] += num(0, 1)
                    i = j + 1
                    continue
                elif i + 1 < n and b[i+1] == 0x5d:  # OSC, skip to ST/BEL
                    j = i + 2
                    while j < n and b[j] != 0x07 and not (b[j] == 0x1b and j+1 < n and b[j+1] == 0x5c):
                        j += 1
                    i = j + (2 if j < n and b[j] == 0x1b else 1)
                    continue
                elif i + 1 < n and b[i+1] == 0x4d:  # ESC M = RI
                    ri(); i += 2; continue
                elif i + 1 < n and b[i+1] == 0x44:  # ESC D = IND
                    lf(); i += 2; continue
                elif i + 1 < n and b[i+1] in (0x37, 0x38, 0x63):
                    i += 2; continue
                else:
                    i += 2; continue
            elif ch == 0x0a:
                lf(); i += 1; continue
            elif ch == 0x0d:
                i += 1; continue
            else:
                i += 1; continue
    return up, down

def load_cast(path):
    lines = open(path).read().splitlines()
    ev = []
    for ln in lines[1:]:
        if not ln.strip():
            continue
        _, code, data = json.loads(ln)
        if code == "o":
            ev.append(("o", data.encode("utf-8", "surrogatepass")))
        elif code == "r":
            c, r = data.split("x")
            ev.append(("r", (int(c), int(r))))
    return ev

for name in sys.argv[1:]:
    S = os.path.dirname(os.path.abspath(__file__))
    ev = load_cast(f"{S}/{name}.cast")
    up, down = replay(ev, 40, 120)
    print(f"== {name}")
    print("  SCROLL-UP events (these are what feed terminal scrollback):")
    if not up: print("    <none>")
    for (t, b, r), c in sorted(up.items()):
        flag = "TOP-ANCHORED (alacritty keeps history)" if t == 1 else "*** TOP-OFFSET (history dropped) ***"
        full = "and full-height (vt100 keeps too)" if (t == 1 and b == r) else "(vt100 drops)"
        print(f"    region {t}..{b} of {r} rows: {c} lines  -> {flag} {full}")
    print("  SCROLL-DOWN events (RI/SD; never produce scrollback in any terminal):")
    if not down: print("    <none>")
    for (t, b, r), c in sorted(down.items()):
        print(f"    region {t}..{b} of {r} rows: {c} lines  {'top-offset' if t != 1 else 'top-anchored'}")
    print()
