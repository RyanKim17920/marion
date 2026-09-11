# S1 — Claude Code's headless `stream-json` control protocol

Recorded **2026-07-31** against **Claude Code 2.1.220**. This directory predates the per-spike
README convention; everything below is quoted from the provenance row for `s1` in
[`../REVIEW.md`](../REVIEW.md), from `MILESTONES.md`, or read directly off the files.

## What the captures are

One headless session, recorded on both sides, exercising the **interrupt protocol**: `initialize`,
a long turn, an `interrupt`, and a follow-up turn afterwards. `REVIEW.md` records the argv verbatim:

```
-p --output-format stream-json --input-format stream-json --include-partial-messages
--verbose --model haiku --allowed-tools ""
```

with the note that **`can_use_tool` never fires here because `--permission-prompt-tool stdio` is
absent** — not because of the empty allowlist, which would make it fire *more*, not less.

| File | What it holds |
|---|---|
| `stdin.jsonl` | what was written to the CLI, each line `{t_rel, msg}` — the `initialize` control request, the user turn, the `interrupt` control request, the follow-up turn |
| `stdout.jsonl` | the CLI's stream, with the `initialize` control request sent |
| `stdout_noinit.jsonl` | the same script **without** `initialize`, so the difference is readable |
| `summary.json` | `argv`, the per-turn `results`, and `stderr` |

The provider was **real** (`REVIEW.md`: *"`total_cost_usd` 0.093935 on the follow-up turn"*), routed
through a local proxy — which is why a resolved model id survives redaction (see below).

## What it answered, and what came after

`MILESTONES.md` marks **Spikes S1–S7 [done]**. S1's one outstanding debt was that the whole
measurement ran over **pipes**: design doc §11 item 1 owed a pty re-confirmation before M1. That was
paid on 2026-08-03 by **S11** (`../s11/`), which replayed *"S1's argv and stdin script **verbatim**"*
over a real pty and found *"identical 38-kind frame sequence, byte-identical interrupt
`control_response`, 36 of 36 non-delta frames byte-identical"* — while the **framing** differed and
`claude -p` was found to **refuse a pty stdin**. Read S1 as the protocol and S11 as the transport
caveat on it.

## Which tests read it

**None.** No Rust test opens a file in this directory; it is cited as evidence in doc comments and
prose, which is why the ledger in [`../../../spikes/README.md`](../../../spikes/README.md) lists it
as *fixtures only*. The citations:

- `crates/marion-harness/src/claude_code.rs` — *"How a `--output-format stream-json` stream is read
  (`tests/fixtures/s1/`, `s9/`)"*
- `crates/marion-supervisor/src/events.rs` — the frame discriminator, *"on every frame in
  `tests/fixtures/s1/`"*
- `crates/marion-supervisor/src/duplex.rs` — *"One `stream-json` user turn, in the shape
  `tests/fixtures/s1/stdin.jsonl` records"*
- `crates/marion-supervisor/tests/permission_round_trip.rs` — names this directory as what
  `can_use_tool` was designed on **before** that test existed (S9 measured it live)

## Redaction

The gate and the full account are in [`../REVIEW.md`](../REVIEW.md); `s1` has a row in its
provenance table and appears in its accepted-residue list. In brief: recorded **without** an
isolated `HOME` (*"S4 did this; **S1 and S2 did not** — which is why S1 needed surgery"*), so the
pass replaced the home path in `argv`, session ids with stable fakes, the pid with `424242`, and
stubbed the `initialize` catalogues (145 commands, 10 agents, 5 models) and the `system.init`
enumerations; all nine `hook_started`/`hook_response` pairs were kept for their envelope shape with
the bodies replaced by `<REDACTED_HOOK_OUTPUT>`. Deliberately retained: per-event `uuid` fields, and
the resolved model id `Qwen/Qwen3.6-27B` — *"the runs went through a local proxy …, the id is the
evidence that they did"*. **Do not add a capture here without adding its row to `REVIEW.md`.**
