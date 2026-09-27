# S34/pi — pi as a marion child, against a canned local provider, at $0.00

Measured 2026-09-27, macOS darwin 25.5.0, **pi 0.80.2** (`/opt/homebrew/bin/pi`, npm
`@earendil-works/pi-coding-agent`, formerly `badlogic/pi-mono`). Provider: `spikes/s34/canned_provider.py`,
a local OpenAI Chat Completions SSE stub that calls the tool named in `TOOL` when the request offers it
and no tool result is in the history yet, and otherwise answers `TEXT`. MCP server:
`spikes/s34/mcp_report_server.py`, a stdio stub with one tool, `report`. **No vendor endpoint, no
login, no real key. Total spend: $0.00.**

Driver: `python3 spikes/s34/pi_run.py tests/fixtures/s34-pi`. Each scenario runs in its own
directory with `PI_CODING_AGENT_DIR=<run>/agent` holding a `models.json` that names provider
`marion` (`api: openai-completions`, `baseUrl: http://127.0.0.1:<port>/v1`), `PI_OFFLINE=1`, stdin
from `/dev/null`, and `pi -p --mode json --no-extensions --provider marion --model canned-1`.

Redactions: run directories → `<RUN-DIR>`, `$HOME` → `<HOME>`, the interpreter → `<PYTHON>`, UUIDs →
`<UUID-N>` (the same id keeps its number across files), timestamps → `<T>`, the system prompt → its
length, built-in tool descriptions → their lengths, the key → `<KEY>` (it appears nowhere).
`message_update` frames (per-delta echoes of the message `message_end` carries whole) are dropped.

## Files

| file | what it is |
|---|---|
| `pi-report.*` | the launch marion compiles: `--tools read,write,edit,mcp__marion__report -e <run>/marion-pi.js`. `tool_execution_start` names `mcp__marion__report`, `tool_execution_end` pairs it by `toolCallId` with `isError: false`, the stub's `tools/call` names the **bare** `report`, exit 0. `provider-request-1.json`: `tools[]` is exactly those four names |
| `pi-report-iserror.*` | the stub answers `isError: true`: `tool_execution_end.isError: true`, `result.content[0].text` is the server's words, the run continues and exits 0 |
| `pi-provider-500.*` | a provider 500: four requests over ~15 s (`auto_retry_start` ×3), each attempt an `agent_end` with `willRetry: true` and an assistant `stopReason: "error"`, the last with `willRetry: false`, then `auto_retry_end.success: false`. **Exit 0** |
| `pi-provider-401.*` | a 401: no retry, one `agent_end` with `willRetry: false` whose assistant message has `stopReason: "error"`, `errorMessage: "401 canned failure"`. **Exit 0** |
| `pi-retry-then-report.*` | one transient 500, then success: the retried attempt's `agent_end` (`willRetry: false`) carries only the new messages; the errored message is not in it |
| `pi-empty-tools.*` | `--tools ""` and no extension: `tools[]` is **empty** (an empty allowlist is no tools, not all tools) |
| `pi-resume-turn-1.*`, `pi-resume-turn-2.*` | turn one, then `--session <id>` with the id off turn one's `session` frame: turn two's `session` frame repeats the id and its request replays turn one |
| `pi-resume-unknown.*` | `--session <unknown id>`: `No session found matching '…'`, exit 1, no stdout |
| `pi-rpc-steer-mid-tool.*` | `--mode rpc` driven by `spikes/s34/pi_rpc.py`. A `prompt` is sent, then during the (3 s, `slow`) MCP call three more commands go in: a bare `prompt`, which is refused (`success: false`, "Agent is already processing. Specify streamingBehavior…"); a `steer`, which lands as a `user` message after the tool result in the **same turn's next request**; and a `follow_up`, which becomes its own request after the turn's last answer. A second `prompt` after `agent_end` is a new turn in the same process and session. `requests.json` lists each provider request's messages |
| `pi-rpc-abort-mid-tool.*` | `abort` during the MCP call: the call still completes, and the next model request ends `stopReason: "aborted"`, `errorMessage: "Request was aborted."`. The process stays up until stdin closes, then exits 0 |
| `pi-tui.facts.json` | `spikes/s34/pi_tui.py`: the interactive TUI on a 120×40 pty with the same `-e` extension. It sets DECSET 2004 (bracketed paste) and 2026 (synchronized output). A multi-line bracketed paste followed by CR 50 ms later reaches the provider byte-exact as one user message. Idle output was 0 B over 10 s, and busy repaints came at ≤ 88 ms gaps. A paste plus CR written while a tool call runs is folded after the tool result into the same turn. No trust, login or changelog screen appeared on the canned agent dir, and there was no OSC 9;4 progress signal |
| `pi-bridge-missing.*` | the extension naming a server that cannot start: `Failed to load extension … marion's MCP server did not start`, exit 1, no request made |

## What was learned

1. **No MCP client, by design** (README: "No MCP"). An extension is pi's plugin surface, and
   `-e <path>` loads one **for one run**. `--no-extensions` turns off discovery but still loads
   explicit `-e` paths, so nothing is installed into `~/.pi` and the operator's own extensions stay
   out of a canned node. marion's declaration is therefore a document of its own:
   `crates/marion-harness/src/pi_extension.js`, a stdio MCP client for one server, rendered per node
   and named on argv. That is the shape Claude Code's `--mcp-config <path>` already has.
2. **The model sees the names the extension registers.** They use Claude Code's
   `mcp__<server>__<tool>` spelling, which pi passes to the provider verbatim. They also appear
   verbatim in `tool_execution_start.toolName` and `tool_execution_end.toolName`.
3. **`--tools` is an allowlist over built-in and extension tools alike.** Only the named tools
   reach `tools[]`. Unknown names are dropped silently, and an empty list offers nothing. `write`
   and `edit` are separate built-ins, and `bash` is offered only when named.
4. **No approval surface.** pi never asks to run a tool (docs/security.md), so marion's tools run
   headless with no flag at all.
5. **The stream.** A `session` header `{id, cwd, version: 3}` comes first. Then come `agent_start`,
   `turn_start`, `message_start`/`message_end` (`message.role`, `message.content[]`,
   `message.usage`, `message.stopReason`), `tool_execution_start {toolCallId, toolName, args}`,
   `tool_execution_end {toolCallId, toolName, result, isError}`, `turn_end`, and
   `agent_end {messages, willRetry}`. The run ends at the `agent_end` whose `willRetry` is false.
6. **Failures exit 0.** A provider fault is an assistant message with `stopReason: "error"` and
   `errorMessage`, inside the final `agent_end`. Retried attempts show the same shape under
   `willRetry: true`, so only the final `agent_end` is a claim.
7. **Usage per assistant message.** Each `message_end` of `role: "assistant"` carries
   `usage {input, output, cacheRead, cacheWrite}`. `input` already excludes cache reads (pi-ai
   subtracts `cached_tokens`), and the run's spend is the sum.
8. **Resume** is `--session <id>`, where the id is the `session` frame's `id`. Sessions are keyed by
   `PI_CODING_AGENT_DIR` and cwd. An unknown id exits 1.
9. **stdin is read to EOF when it is not a TTY**, and appended to the prompt. A launch with an open,
   silent stdin hangs, so marion's `Stdio::null()` is load-bearing.
10. **No self-update.** Updates are manual (`pi update`). The only update traffic at startup is
    the version check, and `dist/utils/version-check.js` skips it when `PI_SKIP_VERSION_CHECK` or
    `PI_OFFLINE` is set. These captures ran under `PI_OFFLINE=1`, which also turns off package
    update checks and telemetry. The row carries the narrower `PI_SKIP_VERSION_CHECK=1`, because a
    live node keeps the operator's own packages.
11. **Isolation.** `PI_CODING_AGENT_DIR` relocates `auth.json`, `models.json`, `settings.json`,
    `sessions/` and extensions. It is also where the operator's credentials live, so a live node
    must not relocate it.
12. **`--mode rpc` is a typed control channel.** Commands are JSONL on stdin and events are the
    same frames as `--mode json` plus `response` records. `steer` is a fold: it is delivered after
    the in-flight tool calls finish and before the next model request of the same turn, which is
    `MidTurn::Fold` as S31 measured it on Claude Code's stream-json. `follow_up` queues the message
    as the next turn, and a bare `prompt` during a turn is refused by name. Hazard: in an
    exploratory run, two bare `prompt`s written 50 ms apart **before** the first `agent_start`
    both answered `success: true`, and the second was never delivered. A driver must wait for
    `agent_start` (or use `steer`/`follow_up`) before writing again. Startup to the first response
    took ~3 s with the extension loading. marion does not drive this surface yet; the row stays
    `LaunchOnly` with `Continuation` delivery. rpc is what a typed row would use (see
    `marion_harness::pi`).
13. **The TUI takes a paste the way S31's four do.** Bracketed paste plus `\r` submits, and output
    goes quiet when pi is idle, so the row's interactive delivery is
    `TurnDelivery::bracketed_paste` (`OutputQuiet{1500}`). The native lane `marion pi` is enabled,
    and `native_facade_e2e` drives it in front of the operator's own pi. That run keeps the
    operator's extensions and adds marion's with `-e`.
