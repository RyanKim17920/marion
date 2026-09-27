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
10. **No self-update.** Updates are manual (`pi update`). `PI_OFFLINE=1` turns off the startup
    version check and package update checks, and install/update telemetry with them.
11. **Isolation.** `PI_CODING_AGENT_DIR` relocates `auth.json`, `models.json`, `settings.json`,
    `sessions/` and extensions. It is also where the operator's credentials live, so a live node
    must not relocate it.
