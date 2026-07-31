#!/usr/bin/env python3
import collections, json, re, struct, sys

def records(path):
    buf = open(path, "rb").read()
    i = 0
    while i + 13 <= len(buf):
        tag = buf[i:i+1]
        ts, n = struct.unpack("<dI", buf[i+1:i+13])
        payload = buf[i+13:i+13+n]
        i += 13 + n
        yield tag, ts, payload

def main(path):
    recs = list(records(path))
    outb = b"".join(p for t, _, p in recs if t == b"O")
    print(f"== {path}")
    print(f"records={len(recs)} out_bytes={len(outb)} resizes={[json.loads(p) for t,_,p in recs if t==b'R']}")

    # DECSTBM: CSI [top] ; [bottom] r  and CSI r  (reset)
    stbm = collections.Counter()
    for m in re.finditer(rb"\x1b\[(\d*)(?:;(\d*))?r", outb):
        stbm[(m.group(1).decode(), (m.group(2) or b"").decode())] += 1
    print("DECSTBM regions (top,bottom) -> count:")
    if not stbm:
        print("  <none>")
    for k, v in sorted(stbm.items()):
        top = k[0] or "1(default)"
        bot = k[1] or "rows(default)"
        print(f"  ESC[{k[0]};{k[1]}r  top={top} bottom={bot}  x{v}")

    # broader: any CSI ending in 'r' (incl. DECRESTOREPM CSI ? ... r)
    other = collections.Counter(m.group(0) for m in re.finditer(rb"\x1b\[[0-9;?]*r", outb))
    print("all CSI...r forms:", {k.decode('latin1').replace('\x1b','ESC'): v for k, v in other.items()})

    # DECSET 2026 sync output
    h = len(re.findall(rb"\x1b\[\?2026h", outb))
    l = len(re.findall(rb"\x1b\[\?2026l", outb))
    print(f"DECSET 2026: begin(h)={h} end(l)={l}")
    # coherence: check strict alternation
    seq = [m.group(1) for m in re.finditer(rb"\x1b\[\?2026(h|l)", outb)]
    bad = sum(1 for a, b in zip(seq, seq[1:]) if a == b)
    print(f"  2026 alternation violations={bad} first={seq[:4]} last={seq[-4:] if seq else []}")
    # bytes per frame
    if h:
        frames = re.findall(rb"\x1b\[\?2026h(.*?)\x1b\[\?2026l", outb, re.S)
        sizes = sorted(len(f) for f in frames)
        print(f"  complete frames={len(frames)} median_bytes={sizes[len(sizes)//2] if sizes else 0} max={sizes[-1] if sizes else 0}")

    # alt screen
    for code in (b"1049", b"47", b"1047"):
        hh = len(re.findall(rb"\x1b\[\?" + code + rb"h", outb))
        ll = len(re.findall(rb"\x1b\[\?" + code + rb"l", outb))
        if hh or ll:
            print(f"ALT SCREEN ?{code.decode()}: h={hh} l={ll}")
    # scroll primitives
    for name, pat in [("RI (ESC M)", rb"\x1bM"), ("IND (ESC D)", rb"\x1bD"),
                      ("SU CSI S", rb"\x1b\[\d*S"), ("SD CSI T", rb"\x1b\[\d*T"),
                      ("DECSC ESC7", rb"\x1b7"), ("ED CSI J", rb"\x1b\[\d*J"),
                      ("IL CSI L", rb"\x1b\[\d*L"), ("DL CSI M", rb"\x1b\[\d*M")]:
        c = len(re.findall(pat, outb))
        if c:
            print(f"  {name}: {c}")

if __name__ == "__main__":
    for p in sys.argv[1:]:
        main(p)
        print()
