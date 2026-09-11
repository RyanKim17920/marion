# S4 — `Stop` / `SubagentStop` hook behaviour

Recorded **2026-07-31** against **Claude Code 2.1.220** and **Codex**. This directory predates the
per-spike README convention; everything below is quoted from the provenance row for `s4` in
[`../REVIEW.md`](../REVIEW.md), from `MILESTONES.md`, or read off the files.

## What the captures are

A stop hook run in every mode each harness supports, with **all four sides** of each run kept: the
hook's stdin payload, the resulting `stream-json`, the transcript, and the hook configuration that
produced it.

- `claude-code/` — **three** modes: `none`, `decision: block`, `additionalContext`. Files:
  `hook-input-*.jsonl`, `stream-*.jsonl`, `transcript-*.jsonl`, plus `settings.json` and
  `stop_hook.sh`.
- `codex/` — **four** modes: the same three plus `exit2`. Files: `hook-input-*.jsonl`,
  `stream-*.jsonl`, plus `hooks.json`, `config.toml.hooks-state.snippet` and `stop_hook.sh`.

`REVIEW.md` records the gap and what it costs: *"`exit2` was not recorded and
`claude-code/stop_hook.sh` has no exit-2 branch, so the design doc's exit-2 equivalence claim is
**unfixtured** on Claude Code (design §11 item 13)."* The provider was **real** — short scripted
turns (*"say the word alpha"*).

## What it answered, and what came after

`MILESTONES.md` marks **Spikes S1–S7 [done]**. S4's `Stop` payload is the baseline the later
subagent work was measured against: **S10** (`../s10/`, 2026-08-03) fired `SubagentStop` live and
found its field set *"exactly S4's 11-key `Stop` set plus `agent_id`/`agent_type`/
`agent_transcript_path`"* — with `session_id` and `transcript_path` being the **parent's**, *"so a
hook reading `transcript_path` on a `SubagentStop` reads the wrong file"*. S10 also **corrected** a
reading that had been taken off S4: `num_turns` is a root counter and says nothing about a subagent
re-prompt; `parent_tool_use_id` is the discriminator.

## Which tests read it

**None.** No Rust test opens a file in this directory — it is evidence for the design doc's hook
sections and for the `Stop`-payload claims S10 extends, which is why the ledger in
[`../../../spikes/README.md`](../../../spikes/README.md) lists it as *fixtures only*.

## Redaction

The gate is [`../REVIEW.md`](../REVIEW.md), where `s4` has a provenance row. S4 is the one early
spike that got this right at capture time: *"Already recorded in an isolated scratch `HOME`, so
`<SCRATCH>` / `<UUID>` placeholders were applied at capture time and the enumerated
command/skill/agent sets are the built-in defaults, not the operator's."* The later pass removed the
residual username inside path-encoded project slugs (`-Users-<name>-Desktop-…` →
`-Users-<USER>-Desktop-…`) and replaced the org name in built-in agent descriptions with `<ORG>`.

Two items are deliberately retained and listed in `REVIEW.md`'s residue list: the env-var **name**
`ANTHROPIC_API_KEY` as an `apiKeySource` value in `claude-code/stream-*.jsonl` (a name, never a
value), and the resolved model id `qwen/qwen3.6-27b` — the runs went through a local proxy and the
id is the evidence that they did. **Do not add a capture here without adding its row to
`REVIEW.md`.**
