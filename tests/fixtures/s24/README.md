# S24 — GitHub Copilot CLI as a marion child, against a canned local provider, at $0.00

Measured 2026-09-05, macOS darwin 25.5.0, **copilot 1.0.83** (`/opt/homebrew/bin/copilot`, installed
with `npm i -g @github/copilot@1.0.83`; the 0.0.367 npm had left there has no BYOK, no
`--output-format json` and no `--acp`, so nothing here was measurable before the upgrade).
Provider: a local OpenAI Chat Completions SSE stub on `127.0.0.1` that answers the first turn with a
streamed `tool_calls` fragment set and the next with text. MCP server: a stdio stub exposing one tool,
`report`. **No vendor endpoint, no GitHub login, no real key. Total spend: $0.00.**

The API key handed to every run was `marion-canned-credential-7c1f`, chosen because it is a substring
of nothing the CLI prints: **copilot replaces every occurrence of `COPILOT_PROVIDER_API_KEY`'s value
in its own JSONL with `******`** (an earlier probe with the key `fake` printed `"model":"******-model"`
for a model named `fake-model`). It appears in no file here.

## Files

| file | what it is |
|---|---|
| `copilot-write-then-report.stdout.jsonl` | the run the adapter compiles: `--available-tools=marion-report,create --allow-tool=marion(report) --allow-tool=write`; `create` lands a file (`session.info` `file_created`), then `marion-report` is answered `recorded`, exit 0 |
| `copilot-write-then-report.provider-request-1.json` | that run's first request as the provider saw it — `POST /v1/chat/completions` against a `…/v1` base URL, `stream: true`, `tools[]` naming exactly `create` and `marion-report` (system prompt elided) |
| `copilot-write-then-report.mcp.jsonl` | that run's MCP transcript: `server/discover`, `initialize` (protocol `2025-11-25`), `notifications/initialized`, `tools/list`, one `tools/call` naming the **bare** `report` |
| `copilot-create-denied-without-grant.stdout.jsonl` | the same availability list with `--allow-tool=marion-report` — the *model-facing* spelling used as a pattern — and no `write` grant: **both** `create` and `marion-report` end `success: false, error.code: "denied"`, and the run exits **0** |
| `copilot-report-iserror.stdout.jsonl` | the MCP stub answering `tools/call` with `isError: true`: `tool.execution_complete` carries `success: false, error.message: "MCP server 'marion': refused: not authorized", error.code: "failure"`, and the run exits **0** |
| `copilot-provider-500.stdout.jsonl` | the provider answering 500: five `model.call_failure` retries over 30 s, then `session.error {errorType: "query", statusCode: 500}` and `result.exitCode: 1` |
| `copilot-allow-all-baseline.stdout.jsonl` | the first probe, `--allow-all` and no `--available-tools`: the model is offered eighteen tools including `bash`, `create`, `edit` — the state marion never compiles |

Redactions: run directories → `<RUN-DIR>` (and their fragments, because `assistant.tool_call_delta`
splits argument strings mid-path), UUIDs → `<UUID>`, timestamps → `<TS>`, the two built-in skills'
descriptions → elided, the system prompt → elided with its length.

## What was learned, in the order it bit

1. **BYOK needs an explicit model.** Without `--model`/`COPILOT_MODEL` the CLI exits 1 with
   `BYOK providers require an explicit model` before any request. The adapter refuses first, by name.
2. **Two axes, two flags, two spellings.** `--available-tools` is what the model sees — everything not
   named is withheld and listed in a `session.info` `configuration` frame — and `--allow-tool` is what
   runs without a prompt. The MCP tool is `marion-report` on the first axis and `marion(report)` on
   the second; the built-in file tools are `create`/`edit`/`view` on the first and the *kind* `write`
   on the second. `--allow-tool=create` and `--allow-tool=marion-report` both grant nothing.
3. **In `-p` mode an ungranted call is denied and the run exits 0.** `Permission denied and could not
   request permission from user`, `code: "denied"`, then the model's closing text, then
   `exitCode: 0`. §12's silent-success shape; the stream is the only place it is described.
4. **`view` needs no grant.** With only `marion(report)` allowed, a `view` call ran (and failed on a
   missing file with `code: "failure"`, not `"denied"`). Reads inside the working directory are
   permitted by default; `-C` bounds them.
5. **The CLI holds turn one for its MCP servers.** `session.mcp_server_status_changed` `pending` →
   `connected`, then `session.mcp_servers_loaded`, then `user.message`, on every capture. The
   earlier `/bin/cat` probe showed the ceiling: 60 s, then `status: "failed"` and a tool-less turn.
   That is §6.1 step 8's gate, built into the harness — no readiness marker is compiled.
6. **The report is on `tool.execution_start`, its verdict on `tool.execution_complete`, paired by
   `toolCallId`.** `arguments` is the parsed object; `assistant.tool_call_delta` fragments are not read.
7. **`COPILOT_HOME` relocates config, session state, logs and the sqlite store**; `HOME` is left
   alone because the launcher finds its platform package under `~/Library/Caches/copilot/pkg/`.
8. **The base URL is taken verbatim in marion's `…/v1` form**: the CLI appends `/chat/completions`.

Consumed by `crates/marion-harness/src/copilot.rs`, whose unit tests `include_str!` four of these
streams and assert every claim above that is a claim about bytes.
