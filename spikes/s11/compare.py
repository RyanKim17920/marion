#!/usr/bin/env python3
"""Compare the three S11 transports to each other and to S1's committed pipe capture.

Answers §11 item 1's question in three parts:
  frames    — is the same *set* of frame kinds emitted, in the same order?
  order     — is the collapsed kind-sequence identical across transports?
  boundaries— do read() chunks fall in the same places (bytes per read, frames per read,
              frames split across reads, line terminator)?

Delta runs are collapsed before the order comparison: how many `content_block_delta`
frames arrive before an interrupt lands is a function of when the interrupt was sent, not
of the transport, so an uncollapsed sequence would report a difference that is only timing.
"""
import json
import os
import sys

TRANSPORTS = ["pipes", "pty-out", "pty-out-raw", "pty-in", "pty-all"]
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
S1 = os.path.join(REPO, "tests", "fixtures", "s1", "stdout.jsonl")


def kind(o):
    t = o.get("type")
    if t == "stream_event":
        ev = o.get("event", {})
        et = ev.get("type", "")
        if et == "content_block_delta":
            return "stream_event/%s/%s" % (et, ev.get("delta", {}).get("type", ""))
        if et == "content_block_start":
            return "stream_event/%s/%s" % (et, ev.get("content_block", {}).get("type", ""))
        return "stream_event/%s" % et
    if t == "control_response":
        return "control_response/%s" % o.get("response", {}).get("subtype", "")
    if t in ("system", "result"):
        return "%s/%s" % (t, o.get("subtype", ""))
    if t == "assistant" or t == "user":
        blocks = o.get("message", {}).get("content", [])
        kinds = ",".join(sorted({b.get("type", "?") for b in blocks
                                 if isinstance(b, dict)}))
        return "%s[%s]" % (t, kinds)
    return str(t)


def collapse(seq):
    out = []
    for k in seq:
        if out and out[-1] == k and "content_block_delta" in k:
            continue
        out.append(k)
    return out


def load_run(d):
    p = os.path.join(d, "stdout.jsonl")
    rows = [json.loads(l) for l in open(p) if l.strip()]
    summary = json.load(open(os.path.join(d, "summary.json")))
    chunks = [json.loads(l) for l in open(os.path.join(d, "chunks.jsonl")) if l.strip()]
    return rows, summary, chunks


def load_s1():
    rows = [json.loads(l) for l in open(S1) if l.strip()]
    return [r["msg"] for r in rows]


def boundary_stats(rows, chunks):
    out = [c for c in chunks if c["tag"] == "O"]
    sizes = sorted(c["bytes"] for c in out)
    per_chunk = {}
    for r in rows:
        per_chunk.setdefault(r["chunk"], 0)
        per_chunk[r["chunk"]] += 1
    complete_in = len(per_chunk)
    return {
        "stdout_read_chunks": len(out),
        "stdout_bytes": sum(sizes),
        "read_size_min": sizes[0] if sizes else 0,
        "read_size_median": sizes[len(sizes) // 2] if sizes else 0,
        "read_size_max": sizes[-1] if sizes else 0,
        "reads_at_or_above_4096": sum(1 for s in sizes if s >= 4096),
        "reads_at_or_above_1024": sum(1 for s in sizes if s >= 1024),
        "json_frames": len(rows),
        "reads_that_completed_a_frame": complete_in,
        "reads_that_completed_no_frame": len(out) - complete_in,
        "max_frames_completed_in_one_read": max(per_chunk.values()) if per_chunk else 0,
    }


def main():
    root = sys.argv[1]
    report = {"per_transport": {}, "comparisons": {}}
    seqs = {}
    for t in TRANSPORTS:
        d = os.path.join(root, "run-%s" % t)
        if not os.path.isdir(d):
            continue
        rows, summary, chunks = load_run(d)
        msgs = [r["msg"] for r in rows]
        seqs[t] = collapse([kind(m) for m in msgs])
        ev = summary["events"]
        report["per_transport"][t] = {
            "errors": summary["errors"],
            "exit_code": summary["exit_code"],
            "probes_seen": summary["probes_seen"],
            "non_json_stdout_count": summary["non_json_stdout_count"],
            "line_endings": {"crlf": summary["framing"]["lines_ending_crlf"],
                             "lf_only": summary["framing"]["lines_ending_lf_only"]},
            "latencies": {k: ev.get(k) for k in
                          ("initialize_latency_s", "first_delta_after_s",
                           "interrupt_response_latency_s", "interrupt_to_result_s",
                           "followup_latency_s")},
            "interrupted_result": ev.get("interrupted_result"),
            "followup_result": ev.get("followup_result"),
            "boundaries": boundary_stats(rows, chunks),
            "distinct_frame_kinds": sorted(set(kind(m) for m in msgs)),
        }

    # Latency spread across repeats (§11 item 11: S1's headline numbers are one run).
    import glob
    lat = {}
    for t in TRANSPORTS:
        runs = sorted(glob.glob(os.path.join(root, "run-%s" % t)) +
                      glob.glob(os.path.join(root, "run-%s.rep*" % t)))
        rows = []
        for d in runs:
            try:
                s = json.load(open(os.path.join(d, "summary.json")))
            except OSError:
                continue
            e = s.get("events", {})
            if e.get("interrupt_response_latency_s") is None:
                continue
            rows.append({
                "run": os.path.basename(d),
                "interrupt_response_latency_ms":
                    round(e["interrupt_response_latency_s"] * 1000, 3),
                "interrupt_to_result_ms":
                    round(e["interrupt_to_result_s"] * 1000, 3)
                    if e.get("interrupt_to_result_s") is not None else None,
            })
        if rows:
            lat[t] = rows
    report["latency_repeats"] = lat
    report["s1_headline_ms"] = {"interrupt_response_latency_ms": 0.500,
                                "interrupt_to_result_ms": 1.868,
                                "note": "tests/fixtures/s1/summary.json, one run"}

    base = "pipes"
    for t in TRANSPORTS:
        if t == base or t not in seqs or base not in seqs:
            continue
        a, b = seqs[base], seqs[t]
        report["comparisons"]["%s_vs_%s" % (base, t)] = {
            "collapsed_sequence_identical": a == b,
            "kind_set_identical": set(a) == set(b),
            "only_in_pipes": sorted(set(a) - set(b)),
            "only_in_%s" % t: sorted(set(b) - set(a)),
            "first_divergence": next((i for i, (x, y) in enumerate(zip(a, b)) if x != y),
                                     None if a == b else min(len(a), len(b))),
            "len_pipes": len(a), "len_%s" % t: len(b),
        }

    s1 = load_s1()
    s1_seq = collapse([kind(m) for m in s1])
    report["s1_fixture"] = {
        "path": "tests/fixtures/s1/stdout.jsonl",
        "frames": len(s1),
        "distinct_frame_kinds": sorted(set(kind(m) for m in s1)),
        "collapsed_len": len(s1_seq),
    }
    for t in TRANSPORTS:
        if t not in seqs:
            continue
        report["comparisons"]["s1_vs_%s" % t] = {
            "kinds_in_s1_absent_here": sorted(set(s1_seq) - set(seqs[t])),
            "kinds_here_absent_in_s1": sorted(set(seqs[t]) - set(s1_seq)),
        }

    print(json.dumps(report, indent=2))
    with open(os.path.join(root, "compare.json"), "w") as fh:
        json.dump(report, fh, indent=2)


if __name__ == "__main__":
    main()
