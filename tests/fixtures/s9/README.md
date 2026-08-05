# S9 — the inbound half of Claude Code's control channel

Run 2026-08-03 against **Claude Code 2.1.220** (`claude --version` → `2.1.220 (Claude Code)`),
macOS (darwin 25.5.0), marion's own **CannedServer** (`crates/marion-provider`) and marion's own
stdio MCP bridge (`marion-supervisor mcp`). **No model was called, no API key was used, no paid
tokens**: `ANTHROPIC_BASE_URL` points at `127.0.0.1` and `ANTHROPIC_API_KEY` is empty.

Closes design doc **§11 item 14**.

## ANSWER: the decompiled design was right about every field it named.

`can_use_tool` really is an inbound `control_request` with a **top-level `request_id`**, a
`request.subtype` of `can_use_tool`, a `request.tool_name` and a `request.tool_use_id`. marion's
`root::deny_response` — written from decompilation and never once executed before this run — was
accepted verbatim by a real CLI on the first attempt. **No implementation change was needed.**

What the design *under*-specified is that the request's field set is **not fixed**, and that
`initialize`'s reply is far larger than documented. Both are recorded below.

## What is here

| file | what it is |
| --- | --- |
| `can-use-tool-deny.stdout.jsonl` | every frame the CLI wrote, marion answering **deny** |
| `can-use-tool-deny.stdin.jsonl` | every frame marion wrote on the same run |
| `can-use-tool-allow.stdout.jsonl` / `.stdin.jsonl` | the same run, answering **allow** |
| `can-use-tool-builtin-deny.stdout.jsonl` / `.stdin.jsonl` | the same subtype for a **built-in** `Bash` call |

`tests/fixtures/s1/stdout.jsonl` contains **zero** inbound `control_request` frames, because that
run omitted `--permission-prompt-tool stdio`. These four do; that is the entire point.

## How the ask was provoked without a model

`report` is deliberately **absent** from `root::ROOT_ALLOWED_TOOLS` (§9 rejects `report` on a node
with no contract, and a root has none) while being a real tool marion's own bridge serves. So the
canned provider simply aims the root's first turn at `mcp__marion__report`. The CLI *offers* the
tool — MCP tools are the availability axis — and refuses to run it unasked — `--allowedTools` is
the permission axis — so it asks. **No argv surgery**: marion's production invocation, bridge and
allowlist, unmodified.

The `builtin` capture is the one exception and says so: it rewrites `--tools ""` to `--tools Bash`,
because marion's root is compiled with no built-in tools at all.

## The ask (deny run, verbatim)

```json
{"type":"control_request","request_id":"<UUID-4>","request":{"subtype":"can_use_tool","tool_name":"mcp__marion__report","display_name":"Report","input":{"narrative":"s9 probe: a verb the root may not use"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"mcp__marion__report"}],"behavior":"allow","destination":"localSettings"}],"tool_use_id":"toolu_marion_spawn_1"}}
```

**The field set is not fixed.** The same subtype for a built-in `Bash` call
(`can-use-tool-builtin-deny.stdout.jsonl`) additionally carries `description` and `blocked_path`,
and three `permission_suggestions` (`addRules` with a `ruleContent`, `addDirectories`, `setMode`)
rather than one. Only **`request_id`, `request.subtype` and `request.tool_name`** appear in both.
`marion_supervisor::root::can_use_tool_request` reads those three and nothing else, and
`permission_round_trip.rs` asserts both field sets so a parser that starts depending on either
fails.

`request_id` is a **bare UUID v4**, not the `req_N` form the design's examples use for marion's own
outbound requests. Since the demux map is keyed on the string, that is a naming observation, not a
protocol one — but it does mean marion must not assume its own id scheme on the inbound direction.

## The answers (verbatim)

Deny — the exact string `root::deny_response` produces:

```json
{"response":{"request_id":"<UUID-4>","response":{"behavior":"deny","message":"marion: no permission answerer in M1; the root's Blocked bound expired"},"subtype":"success"},"type":"control_response"}
```

Allow:

```json
{"response":{"request_id":"<UUID-4>","response":{"behavior":"allow","updatedInput":{"narrative":"s9 probe: a verb the root may not use"}},"subtype":"success"},"type":"control_response"}
```

Note the envelope's own `subtype` is **`success` in both cases**. It reports that an answer was
produced, not that permission was granted; the grant/refusal lives in `response.response.behavior`.

`updatedInput` is **optional** — a bare `{"behavior":"allow"}` was measured running the tool with
the model's original input. marion sends it anyway, because rewriting arguments is what the field
is for.

## What each outcome does to the run

**Deny.** The CLI turns the denial into a `tool_result` with `is_error: true` whose `content` is
marion's `message` verbatim, tags it `tool_result_meta[].non_execution_kind: "permission-rule"`,
lists the call under the terminal frame's `permission_denials`, and **the turn continues**:
`terminal_reason: "completed"`, `is_error: false`, exit **0**. §9's rule — *"block until the root's
bound expires, then deny the pending request and let the root proceed — do not kill the root"* — is
therefore a recorded behaviour, not a design claim. It had never been executed before this run.

**Allow.** The tool actually runs. The `tool_result` carries a string marion's *own bridge* returns —
so the answer reached the MCP server, not merely the CLI. No `permission_denials`,
`terminal_reason: "completed"`, exit 0.

That string, in this recording, is `report recorded`, and **marion no longer produces it**: §5.4
rejects `report` on a root, and the bridge now answers that rule's refusal
(`bridge::REPORT_ON_A_ROOT`). The recording is left as recorded — it is a capture of the pinned
2.1.220, and re-recording it on another `claude` would move the whole capture, CLI version and all,
off the version everything above is attributed to. What is measured here is the *channel*, which is
unchanged; the superseded answer is declared and asserted in
`permission_round_trip.rs`'s `SUPERSEDED_ALLOW_ANSWER`, which fails both if this file stops saying it
and if the bridge starts saying it again.

## Also measured, and not previously written down

The `control_response` to `initialize` is **~30 kB**. It carries the operator's entire slash-command
catalogue with descriptions, the subagent list, the model list with prices, `output_style`,
`available_output_styles`, `account.tokenSource` and the CLI's `pid`. The design calls `initialize`
"optional — needed only to register SDK-side hooks/MCP or read the session catalogue"; that reply is
the session catalogue, and it is large. marion sends `initialize` on every root launch as its
event-loop round trip (see `root.rs`), so this crosses the pipe every run.

## Reproducing

```sh
cargo test -p marion-supervisor --test permission_round_trip
```

Needs real `claude` (2.1.220) on `PATH`. No `codex` child is involved. The tests **assert against
these files**; to rewrite them from a fresh run:

```sh
MARION_S9_RECORD=1 cargo test -p marion-supervisor --test permission_round_trip -- --test-threads=1
```

That flag is why these are recordings: no line here was typed by hand, and the test that pastes two
of them into its assertions also checks the paste is still present in the file.

## Redaction

- Home paths → `<HOME>`; the per-run scratch directory → `<SCRATCH>` (both with and without macOS's
  `/private` prefix); the root's `AgentId` → `<AGENT-ID>`.
- UUIDs → `<UUID-1>`, `<UUID-2>`, … **numbered by first appearance**. S6 flattened every uuid to a
  single `<UUID>`; that would destroy the one correlation this fixture exists to record — that the
  `control_response` names the same `request_id` the `control_request` did — so the numbering is a
  deliberate deviation.
- **Reduced, not merely scrubbed** (as S6 reduced its provider request log): the `system/init`
  frame's `slash_commands`, `skills`, `agents`, `plugins` and `memory_paths`, and the `initialize`
  reply's `commands`, `agents`, `models`, `available_output_styles`, `account` and `pid`, are
  replaced with `"<REDACTED-machine-specific>"`. The keys are kept so the shape still reads. None
  of it is evidence about this channel and all of it is specific to the operator's machine.
- The two reduced frames were re-serialised, so their key order is normalised. Every other line is
  byte-verbatim apart from the substitutions above.

## Still open after this

- **Hook callbacks and `request_user_dialog`** — the other two inbound `control_request` kinds §11
  item 14 names — are **still unmeasured**. Nothing here provokes either: hook callbacks need
  SDK-side hooks registered through `initialize` (marion sends `hooks: {}`), and no probe has found
  a headless path that triggers a user dialog.
- **`control_cancel_request`** is untested in either direction.
- Everything here is one machine, one CLI version, one run per outcome.
