#!/usr/bin/env python3
"""Byte-equality of frames across transports, on the redacted fixtures.

The kind-sequence comparison in compare.py answers "same frames, same order". This
answers the stronger question: are the frame *bytes* the same? Four things legitimately
differ between two runs of the same script and are normalised away — per-run UUIDs (already
`<UUID-N>` in the fixture, flattened to `<U>` because the numbering is per-run), wall-clock
`timestamp` strings, wall-clock durations, and the canned provider's counting text (how far
the count got before the interrupt landed is timing, not transport).

`content_block_delta` frames are excluded for the same reason: their *count* is timing.

    python3 frame_equality.py            # compares tests/fixtures/s11/*
"""
import json
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(
    os.path.abspath(__file__)))), "tests", "fixtures", "s11")

UU = re.compile(r"<UUID-\d+>")
COUNT = re.compile(r'(?:\d+\\n)+\d*')
TS = re.compile(r'"timestamp":\s*"[^"]*"')
NUMS = ("duration_ms", "duration_api_ms", "time_to_request_ms", "num_turns",
        "ttft_ms", "ttft_stream_ms")


def norm(o):
    s = json.dumps(o, sort_keys=True)
    s = UU.sub("<U>", s)
    s = TS.sub('"timestamp":"<TS>"', s)
    for k in NUMS:
        s = re.sub(r'"%s":\s*\d+' % k, '"%s":0' % k, s)
    return COUNT.sub("<COUNT>", s)


def frames(t):
    p = os.path.join(ROOT, t, "stdout.jsonl")
    ks = [norm(json.loads(l)["msg"]) for l in open(p) if l.strip()]
    return [k for k in ks if '"text_delta"' not in k]


def main():
    ts = sys.argv[1:] or ["pipes", "pty-out", "pty-out-raw"]
    got = {t: frames(t) for t in ts}
    base = ts[0]
    out = {"normalised_non_delta_frames": {t: len(v) for t, v in got.items()},
           "identical_to_%s" % base: {}}
    for t in ts[1:]:
        same = got[base] == got[t]
        out["identical_to_%s" % base][t] = same
        if not same:
            for i, (x, y) in enumerate(zip(got[base], got[t])):
                if x != y:
                    out.setdefault("first_diff", {})[t] = {"index": i,
                                                           base: x[:400], t: y[:400]}
                    break
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
