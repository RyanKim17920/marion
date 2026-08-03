# S10 — `SubagentStop`, fired live

Run 2026-08-03 against **Claude Code 2.1.220** (`claude --version` → `2.1.220 (Claude Code)`),
macOS (darwin 25.5.0), driven by `spikes/s10/canned_provider.py`. **No model was called, no real
credential was used, no paid tokens**: `ANTHROPIC_BASE_URL` points at `127.0.0.1` and
`ANTHROPIC_API_KEY` is the literal string `s10-canned-no-real-key`. Every run reports
`total_cost_usd: 0` and `output_tokens: 0`. **Total spend: $0.00.**

Closes design doc **§11 item 2** ("`SubagentStop` live confirmation — static-only so far").

## ANSWER: it fires, and the three statically-inferred fields are all really there.

`agent_id`, `agent_type` and `agent_transcript_path` are present on every `SubagentStop` fire and
absent from every `Stop` fire. `{"decision":"block","reason":…}` **does** re-prompt a stopping
subagent, by exactly the mechanism the design records for the parent `Stop`. `stop_hook_active`
appears, `false` then `true`.

**One design claim does not transfer.** §7.6 offers `num_turns` 1→2 as the observable that a block
landed. That is a *root* counter: it is **2 in all three runs here**, including the run where no
hook was registered at all, and including the run where the subagent took an extra turn. A
subagent re-prompt is invisible in `num_turns`. The observable that does exist is the injected
`user` frame, and it is a better one because it carries `parent_tool_use_id`.

## What is here

| file | what it is |
| --- | --- |
| `hook-input-none.jsonl` | every hook fire, hook returning empty — 1 `SubagentStop` + 1 `Stop` |
| `hook-input-block.jsonl` | the same, hook returning `{"decision":"block",…}` — 2 `SubagentStop` + 1 `Stop` |
| `stream-none.jsonl` / `stream-block.jsonl` | every `stream-json` frame of those two runs |
| `settings-none.json` / `settings-block.json` | the **exact** settings file that made it fire |
| `settings-badshape.json` / `stream-badshape.jsonl` | the negative control (below) |

There is no `hook-input-badshape.jsonl` because the control produced **zero** fires; that absence
is the measurement.

## The payload, verbatim (`SubagentStop`, first fire)

```json
{"session_id":"<UUID-1>","transcript_path":"<SCRATCH>/claude-config/projects/<SCRATCH-SLUG>-cwd/<UUID-1>.jsonl","cwd":"<SCRATCH>/cwd","prompt_id":"<UUID-2>","permission_mode":"bypassPermissions","agent_id":"<AGENT-ID-1>","agent_type":"general-purpose","effort":{"level":"high"},"hook_event_name":"SubagentStop","stop_hook_active":false,"agent_transcript_path":"<SCRATCH>/claude-config/projects/<SCRATCH-SLUG>-cwd/<UUID-1>/subagents/agent-<AGENT-ID-1>.jsonl","last_assistant_message":"ok","background_tasks":[],"session_crons":[]}
```

Fourteen keys. Eleven are the `Stop` set the design already fixtured in `s4`
(`session_id`, `transcript_path`, `cwd`, `prompt_id`, `permission_mode`, `effort`,
`hook_event_name`, `stop_hook_active`, `last_assistant_message`, `background_tasks`,
`session_crons`); the three additions are exactly `agent_id`, `agent_type` and
`agent_transcript_path`. **Nothing else rides along** — the static reading was complete, not merely
correct. The paired `Stop` fire in the same run is the eleven-key set with none of the three.

Three things marion should note about the additions:

- **`session_id` is the *parent's*.** The subagent does not get one of its own, so `session_id`
  cannot key a node. `agent_id` (17 lowercase hex, no dashes — a different shape from the UUIDs
  everywhere else) is the only per-child identifier in the payload.
- **`transcript_path` is also the parent's**, and `agent_transcript_path` is the child's, at
  `<parent transcript dir>/<session_id>/subagents/agent-<agent_id>.jsonl`. A hook that reads
  `transcript_path` on a `SubagentStop` fire reads the wrong file.
- **`agent_type` is the `subagent_type` argument** as passed to the tool (`general-purpose` here),
  not a resolved or canonicalised name.

`agent_id` is the same value the root sees as `system/task_started`'s `task_id`, as
`task_notification`'s `task_id`, and as `agentId` in the `tool_result` — one id, four names.

## What `decision: block` does to a subagent

It re-prompts it, and the injected turn is a **real `user` message** on the wire, exactly as the
design records for the parent-level `Stop`:

```json
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Stop hook feedback:\nmarion s10: report before you stop."}]},"parent_tool_use_id":"toolu_s10_task_1", …}
```

**`parent_tool_use_id` is set to the `Agent` call's `tool_use_id`.** That is what distinguishes a
subagent re-prompt from a root one (`parent_tool_use_id: null`), and it is the frame marion should
watch. The provider log confirms delivery reached the model and not just the CLI: the subagent's
next request carries `{"role":"user","content":[{"type":"text","text":"Stop hook feedback:\nmarion
s10: report before you stop.","cache_control":{"type":"ephemeral"}}]}` appended after its own
`"ok"` — so the reason arrives as conversation, in-band, with a cache breakpoint on it.

The subagent then answered again, stopped again, and `SubagentStop` fired a **second** time with
`stop_hook_active: true` and the new `last_assistant_message`. `agent_id` is **identical across
both fires** — it is the child's identity, not the fire's, so marion can correlate the guard flag
to the node. The root's `tool_result` then carried the *second* answer, so the re-prompt is not
merely delivered, it replaces the reported result.

Sequence in `stream-block.jsonl`: `task_started` → subagent `assistant` → `hook_started`/
`hook_response`(SubagentStop, `output` = marion's block JSON) → `user` "Stop hook feedback:…" →
subagent `assistant` → `hook_started`/`hook_response`(SubagentStop, empty) → `task_updated`
`status: completed` → `task_notification` → root `tool_result`.

`--include-hook-events` puts `hook_started` and `hook_response` frames in the stream, and
`hook_response.output` echoes the hook's stdout verbatim. marion gets an ack of its own decision
on the same channel it watches everything else on.

## The negative control (this is why "it never fired" is not evidence)

`settings-badshape.json` registers three deliberately wrong things at once: `SubagentStop`
**single**-nested (inner `hooks` array omitted), plus the misspelled keys `SubAgentStop` and
`subagent_stop` in the correct double nesting. The run is otherwise byte-identical in setup.

Result: **zero hook fires, zero `hook_started` frames, no warning on stderr, `is_error: false`,
exit 0**, and the same `num_turns: 2` as the working runs. A wrong hook shape is indistinguishable
from a working shape whose event never occurred. Anyone concluding `SubagentStop` is broken must
show a *correct* settings file first; this pair is the before/after.

## Reproducing

```sh
spikes/s10/run_probe.sh <outdir> none      # hook returns empty
spikes/s10/run_probe.sh <outdir> block     # hook returns {"decision":"block",…}
spikes/s10/run_probe.sh <outdir> badshape  # negative control
spikes/s10/redact.py <outdir> <label> tests/fixtures/s10
```

Needs real `claude` (2.1.220) and `python3` on `PATH`. Needs **no** network, no API key and no
`codex`. `run_probe.sh` runs the CLI under a throwaway `CLAUDE_CONFIG_DIR` and a throwaway `cwd`
with `--setting-sources ""`, so the operator's real `~/.claude` is never read and never written —
the hook exists only inside the run directory.

How the subagent was provoked without a model: the canned provider aims the root's first turn at
the built-in subagent tool with `run_in_background: false`, and answers the child's turn with the
single word `ok` so it stops immediately. **In 2.1.220 that tool is advertised to the model as
`Agent`, while `--tools` and `system/init` both call it `Task`.** A `tool_use` block naming
*either* name is honoured — that was measured separately, and a run driven with the name `Task`
produced the same subagent and the same `SubagentStop` fire. The name that matters is the one to
**match on**: a provider keying off `"Task" in tools[].name` never matches, silently serves the
wrong turn, and looks exactly like "the subagent tool is unavailable". That cost one run here.

## Redaction

- `$HOME` → `<HOME>`; the per-run scratch directory → `<SCRATCH>` (both with and without macOS's
  `/private` prefix). Claude Code also embeds path-slugs with `/` → `-`; those became
  `<HOME-SLUG>` / `<SCRATCH-SLUG>`.
- UUIDs → `<UUID-1>`, `<UUID-2>`, … and agent ids → `<AGENT-ID-1>`, … **numbered by first
  appearance**, following S9 rather than S6. The correlations this fixture exists to record — that
  the two `SubagentStop` fires name the *same* `agent_id`, and that `agent_transcript_path` is
  built from the *parent's* `session_id` — are destroyed by flattening.
- Nothing was reduced or re-serialised. Every other byte is as the CLI wrote it. The hook records
  its stdin as raw text and `redact.py` substitutes into that string, so no line here has been
  through a parse/re-emit round trip.

## Still open after this

- **One machine, one CLI version, one `subagent_type`** (`general-purpose`), one run per mode.
  A custom `.claude/agents/*.md` type was not tested, so whether `agent_type` reports the
  file-defined name is unverified.
- **`run_in_background: true` was not tested.** The probe forces synchronous execution. Whether
  `SubagentStop` fires for a backgrounded subagent, and whether the root can exit first and lose
  the fire, is unmeasured — and that is precisely the mis-gating case §7.6 cares about.
- **Nested subagents** (a subagent spawning its own) were not tested, so it is unproven whether
  `agent_id` in the payload is the stopping agent or the outermost one.
- **Blocking more than twice** was not tested; the hook obeys the `stop_hook_active` guard as
  marion does, so what the CLI does to a hook that ignores the guard is unmeasured.
- The subagent here ran with a canned model and no tools. A subagent that had *work* pending when
  blocked may behave differently; nothing here speaks to that.
