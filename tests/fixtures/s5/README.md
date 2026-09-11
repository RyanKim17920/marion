# S5 — `codex app-server` multi-client semantics

Recorded **2026-07-31** against **codex-cli 0.146.0** (`--listen ws://`). This directory predates
the per-spike README convention; everything below is quoted from the provenance row for `s5` in
[`../REVIEW.md`](../REVIEW.md), from the design doc, or read off the files.

## What the captures are

Three probes against one app-server, each committed as its driver plus its capture, and one method
dump:

| File | Probe |
|---|---|
| `probe1-late-join.mjs` / `.json` | a second client joining **after** a thread is already running |
| `probe2-unsubscribe-isolation.mjs` / `.json` | whether `thread/unsubscribe` is **per connection** |
| `probe3-midturn-attach.mjs` / `.json` | attaching **mid-turn**, with the full event stream captured for *both* clients |
| `app-server-methods-0.146.0.json` | the method-list error dump for 0.146.0 |

The provider was **real** — `REVIEW.md`: *"probes run turns; probe3 captures live
`item/agentMessage/delta`"*, 93 and 87 events across the two clients.

## What it answered, and what it did not

The design doc reads the attach sequence off this corpus (*"Attach sequence (S5, verified on 0.146.0
and in `rust-v0.146.0` source)"* — `thread/resume` **is** the attach verb), and `MILESTONES.md`
marks **Spikes S1–S7 [done]**. Two limits are stated in the design doc and should be carried with
any claim sourced here, because neither is visible from the captures alone:

- **No S5 probe issues `turn/steer` at all** — it appears in this directory only in
  `app-server-methods-0.146.0.json`, the method list.
- **No S5 probe exercises an approval**; probe3's turn is `agentMessage/delta` only. The design doc
  marks that reading `UNVERIFIED`, and lists Codex multi-client semantics as *"largely
  source-derived, not measured"*.

## Which tests read it

**None.** No Rust test opens a file in this directory — it is evidence for the design doc's
app-server sections, which is why the ledger in
[`../../../spikes/README.md`](../../../spikes/README.md) lists it as *fixtures only*.

## Redaction

The gate is [`../REVIEW.md`](../REVIEW.md), where `s5` has a provenance row. The pass replaced the
username inside the path-encoded scratch cwd and `runtimeWorkspaceRoots` with `<USER>`, and verified
that the load-bearing numbers survived unchanged: *"Event counts (93 / 87), `attachAtMs`, and method
histograms verified unchanged."*

Deliberately retained, and listed in `REVIEW.md`'s residue list: message ids (`msg_…`) and thread
ids, because probes 1–3 correlate their events by them, and the per-event `uuid` fields, which are
random per run and carry no host meaning. **Do not add a capture here without adding its row to
`REVIEW.md`.**
