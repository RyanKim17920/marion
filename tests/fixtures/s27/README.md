# S27 — Cline CLI as a marion child, against a canned local provider, at $0.00

Measured 2026-09-05, macOS darwin 25.5.0, **cline 3.0.61** (`/opt/homebrew/bin/cline`, installed with
`npm i -g cline@3.0.61`; the npm package is a Node launcher around a Bun-compiled Mach-O at
`bin/.cline`, with `@cline/core 0.0.82` as the runtime). Provider: a local OpenAI Chat Completions SSE
stub on `127.0.0.1` that answers the first turn with a streamed `tool_calls` fragment set and the next
with text. MCP server: a stdio stub exposing one tool, `report`. **No Cline account, no real key.
Total spend: $0.00.** One probe (item 14) did reach a vendor endpoint: with the providers document
moved out of the data dir, cline fell back to its own `cline` provider and made two unauthenticated
HTTPS requests to `api.cline.bot`, both answered 401. No other run left loopback.

The API key handed to every run was `marion-canned-credential-27cc`. It appears in no file here;
`spikes/s27/redact.py` refuses to write a fixture that contains it. Unlike copilot, cline does not
mask the key itself: it is written in clear into `providers.json` and sent as `Authorization`
(recorded as `<redacted:36 chars>`), and it never reaches stdout, stderr, the session store or
`cline history`.

## Files

| file | what it is |
|---|---|
| `cline-report-ok.stdout.jsonl` | the run the adapter compiles: `cline --json -c <work> --config <cfg> --data-dir <data> "<prompt>"` with `CLINE_DIR`/`CLINE_DATA_DIR`/`HOME` relocated; the canned model calls `marion__report`, the MCP stub answers `recorded`, the model closes with text, exit **0** |
| `cline-report-ok.provider-request-1.json` | that run's first request as the provider saw it — `POST /v1/chat/completions` against the `…/v1` base URL, `stream: true`, `tool_choice: "auto"`, `tools[]` with **26** function entries, `marion__report` kept verbatim and the other 25 elided to their names; system prompt elided to its length (4375 chars, mentions neither "mcp" nor the tool) |
| `cline-report-ok.mcp.jsonl` | that run's MCP transcript: `initialize` (protocol `2024-11-05`, client `@cline/core`), `notifications/initialized`, `tools/list`, one `tools/call` naming the **bare** `report` |
| `cline-report-ok.history.json` | `cline history --json` run afterwards in the same tree: the only place the session id is printed (`sessionId`, `exitCode`, `status`, `messagesPath`) |
| `cline-report-ok.meta.json` | argv, added env, exit code, `pgrep -fl cline` before/after (both empty), the `data/` tree the run left, the `home/` tree it left (empty) |
| `providers.json` | `<data>/settings/providers.json`, byte-for-byte what `cline auth openai-compatible -k … -m … -b …` wrote into an empty `--data-dir` (key → `<API-KEY>`) |
| `cline_mcp_settings.json` | `<data>/settings/cline_mcp_settings.json`, the shape `cline mcp install marion --yes --transport stdio -- <cmd> <args>` wrote: `mcpServers.marion.transport.{type,command,args}` |
| `cline-report-iserror.stdout.jsonl`, `.mcp.jsonl`, `.meta.json` | the MCP stub answering `tools/call` with `isError: true`: `content_end` carries `output.isError: true` and the refusal text, the model is asked again, the run exits **0** |
| `cline-provider-500.stdout.jsonl`, `.meta.json` | the provider answering 500 three times in ~6 s: `hook_event agent_error`, `agent_event error {recoverable: false}`, `run_result finishReason: "error"`, exit **1** |
| `cline-report-denied-no-auto-approve.stdout.jsonl`, `.meta.json` | `--auto-approve false` headless: the MCP call is **not** made, `content_end` carries `output.error: "Tool \"marion__report\" requires approval in a TTY session -- NOT a tool or system failure…"`, an `agent_event error {recoverable: true}` follows, the model is asked again, exit **0** |
| `cline-resume-id-json.stderr.txt`, `.meta.json` | `--id <session-id>` with `--json` and a prompt: one `{"type":"error"}` frame **on stderr**, nothing on stdout, exit **1**, no request reaches the provider |
| `cline-resume-id-text.stderr.txt`, `.meta.json` | the same without `--json`: `interactive mode requires a TTY`, exit **1** |
| `cline-env-only-spawns-hub-daemon.meta.json`, `.lock.json` | the first probe, relocated by env only (no `--data-dir` flag): exit 0, but `pgrep` afterwards shows `bin/.cline --cline-hub-daemon --host 127.0.0.1 --port 25463 --pathname /hub`, and `<data>/locks/hub/production.json` records its pid, url and auth token |
| `cline-flags-only-leaks-home.meta.json` | relocated by `--config`/`--data-dir` flags only: no daemon, but `$HOME/.cline/data/db/sessions.db` is written anyway |
| `cline-mcp-settings-path-env.stdout.jsonl`, `.mcp.jsonl`, `.meta.json` | the MCP document **outside** the data dir, named by `CLINE_MCP_SETTINGS_PATH`: honoured — `marion__report` in `tools[]`, `tools/call` reaches the stub, exit 0, identical stream to `report-ok` |
| `cline-provider-settings-path-env-ignored.stdout.jsonl`, `.stderr.txt`, `.meta.json`, `.default-providers.json`, `.vendor-requests.json` | `providers.json` **outside** the data dir, named by `CLINE_PROVIDER_SETTINGS_PATH`: **ignored** — cline wrote a fresh `<data>/settings/providers.json` selecting `cline` / `anthropic/claude-fable-5.1`, POSTed twice to `https://api.cline.bot/api/v1/chat/completions` (401 `Unauthorized`, logged as `AI_APICallError`), emitted `agent_event error {errorClass: "auth", recoverable: false}` and `run_result finishReason: "error"`, exit **1**; the canned provider saw no request |

Redactions: run directories → `<RUN-DIR>` (with and without the macOS `/private` prefix), the
checkout → `<REPO>`, timestamps → `<TS>`, the `<epoch-ms>_<slug>` session id → `<SESSION-ID>`,
`agent_…`/`conv_…`/`hub_…` ids → `<AGENT-ID>`/`<CONV-ID>`/`<HUB-ID>`, `msg_…` → `<MSG-ID>`, team
ids/names → `<TEAM-ID>`/`<TEAM-NAME>`, the hub auth token → `<HUB-AUTH-TOKEN>`, the system prompt
→ elided with its length, the 25 built-in tool definitions → names kept, bodies elided.

## What was learned, in the order it bit

1. **3.0.61 is not the Cline the task description remembered.** There is no `task` subcommand, no
   `-y`, no `instance`, no `cline-core` gRPC host. Headless is the bare positional prompt (act
   mode, auto-approve **on by default**), `--json` for the stream, `--auto-approve <true|false>`,
   `-c <cwd>`, `--id <session-id>`, `-P/-k/-m` for provider/key/model, `--config <dir>`,
   `--data-dir <dir>`, and a `hub` daemon. `cline -y` is an unknown option.
2. **Tools are native OpenAI `tools[]`, not a prose/XML system prompt.** The stub had to be
   ready to emit `<use_mcp_tool>` text; it never needed to. Every request carries
   `tools[]` with 26 `type: "function"` entries and `tool_choice: "auto"`; the system prompt
   (4.4 KB) never mentions MCP or the server. The installed bundle contains no `use_mcp_tool`
   string at all. The stub emits a streamed `tool_calls` delta and cline executes it.
3. **The MCP tool is `marion__report` to the model and bare `report` on the wire** — double
   underscore, `<serverName>__<toolName>`. The stub looked the name up in the `tools[]` array
   cline itself sent before emitting the call; a blind guess would have passed with the bridge
   disconnected.
4. **Relocation is three env vars, and the providers file is discoverable by letting the CLI
   write it.** `CLINE_DIR` roots config (default `~/.cline`), `CLINE_DATA_DIR` roots state
   (default `<CLINE_DIR>/data`) and holds `settings/providers.json` and
   `settings/cline_mcp_settings.json` (the path resolver also reads `CLINE_PROVIDER_SETTINGS_PATH`
   and `CLINE_MCP_SETTINGS_PATH`; only the second one works end to end — item 14); `HOME` matters because rules and
   skills are read from `~/.agents`, `~/Documents/Cline` and `~/Cline`. No env var named
   `CLINE_DATA_DIR`/`CLINE_DIR` was guessed: both were read off `@cline/shared/dist/storage`.
   `cline auth openai-compatible -k <key> -m <model> -b <url> --config … --data-dir …` runs
   non-interactively, exits 0, prints `Provider configured`, and writes exactly `providers.json`
   here. `cline mcp install marion --yes --transport stdio -- <cmd> <args…>` writes exactly
   `cline_mcp_settings.json`. Both were reproduced by hand for every run and honoured.
5. **Env-only relocation spawns a hub daemon that outlives the run.** The first run with no hub
   on the default port forked `bin/.cline --cline-hub-daemon … --port 25463`, wrote
   `<data>/locks/hub/production.json` (pid, `ws://127.0.0.1:25463/hub`, an auth token) and left
   it running; it was still alive ten minutes later. Later runs with other data dirs found the
   port taken, logged `Falling back to local runtime host (compatible_hub_unavailable)`, and
   spawned nothing — but the first one in any clean environment will. Two measured ways to stop
   it: `kill -TERM <pid from production.json>` (the daemon removes `production.json` on the way
   out) and `cline doctor fix` (`killed stale hub daemons 1`). The daemon also answers plain
   HTTP on `/health`.
6. **The `--data-dir` flag is what turns the daemon off.** The CLI sets `sandbox: !!dataDir`
   and `forceLocalBackend: … || sandbox`; with the flag present no daemon is forked and no
   `locks/` directory appears. But flags **alone** leak: `--config`/`--data-dir` without the env
   vars still wrote `$HOME/.cline/data/db/sessions.db`. **The adapter must pass both**: the
   three env vars *and* `--config <cfg> --data-dir <data>`. That combination left `home/` empty,
   `pgrep` empty, and the real `~/.cline` untouched (mtimes identical before and after every run).
7. **The stream has two frame families, and the session id is in neither.** `hook_event`
   frames (`agent_start`, `tool_call`, `tool_result`, `agent_end`, `agent_error`) carry
   `agentId`/`taskId` (`agent_…`/`conv_…`, not the session id); `agent_event` frames carry the
   turn. The report is `agent_event.event {type: "content_start", contentType: "tool", toolName,
   toolCallId, input}` — `input` already the parsed object, no deltas to assemble — and its
   verdict is `content_end` with the same `toolCallId`, `output` the MCP result object
   (`{content, isError}`) on success and `{error: "…"}` on a refusal, plus a sibling `error`
   string. The final frame is `run_result {finishReason: "completed"|"error", text, model}`; the
   exit code follows `finishReason`. The session id (`<epoch-ms>_<slug>`) appears only as the
   `data/sessions/<id>/` directory name and in `cline history --json`, run afterwards in the
   same tree.
8. **`isError: true` is not an error to cline.** It is forwarded to the model as a tool result
   with `isError: true`, the model gets another turn, `run_result` says `completed`, exit 0.
   §12's silent-success shape; `content_end.output.isError` is the only place it is described.
9. **A provider 500 is retried twice, then fatal.** Three POSTs over ~6 s, then `hook_event
   agent_error`, `agent_event error {message: "canned failure", recoverable: false}` (the
   provider's error body text), `run_result finishReason: "error"`, exit 1. The 35 s the first
   such run took was the daemon spawn of item 5, not retry backoff.
10. **`--auto-approve false` headless does not hang and does not call the tool.** There is no
    prompt to wait on: the call is answered inside cline with
    `Tool "marion__report" requires approval in a TTY session -- NOT a tool or system failure`,
    logged as `agent_event error {recoverable: true}`, and the model is asked again. Exit 0.
    The MCP transcript for that run has `initialize`/`tools/list` and no `tools/call`.
11. **Headless resume does not exist in 3.0.61.** `--id <session-id>` sets the startup target to
    `chat`, which the entry point turns into `interactive: true, prompt: undefined` before any
    other check — so with `--json` it exits 1 with `JSON output mode requires a prompt argument
    or piped stdin (interactive mode is unsupported)` (a JSON frame **on stderr**), and without
    `--json` with `interactive mode requires a TTY`. Prompt position and piping on stdin make no
    difference; the prompt is discarded first. A marion resume would have to replay from the
    `data/sessions/<id>/<id>.messages.json` the run leaves behind, or not be offered.
12. **The built-in tool set could not be narrowed.** The model is offered `read_files`,
    `search_codebase`, `run_commands`, `fetch_web_content`, `editor`, `ask_question`,
    `spawn_agent` and nineteen `team_*` tools alongside `marion__report`. Writing
    `disabledTools` / `tools.{bash,editor}.enabled=false` into `<data>/settings/global-settings.json`
    changed nothing on the wire: the same 26 names, in the same order. No CLI flag restricts
    tools. `-s <system-prompt>` replaces the system prompt but not `tools[]`. The adapter cannot
    compile a withheld-tools state; it can only observe what the model does.
13. **The base URL is taken verbatim in marion's `…/v1` form**: the CLI appends
    `/chat/completions`. `User-Agent` is `ai-sdk/openai-compatible/3.0.37 … runtime/bun/1.3.13`.
    stderr carries AI SDK deprecation warnings on every run (`providerOptions key
    'openai-compatible'`), so a supervisor must not treat non-empty stderr as failure.
14. **`CLINE_MCP_SETTINGS_PATH` works; `CLINE_PROVIDER_SETTINGS_PATH` is read by the path resolver
    but not by the provider store.** With the MCP document outside the data dir and only the env
    var naming it, the run was byte-for-byte the `report-ok` stream and the stub got its
    `tools/call`. With `providers.json` outside the data dir the same way, cline found no
    providers file where it looked, **wrote a default one** in `<data>/settings/` selecting its
    own `cline` provider and `anthropic/claude-fable-5.1`, and made two HTTPS POSTs to
    `https://api.cline.bot/api/v1/chat/completions` with no credential — 401 `Unauthorized`,
    `errorClass: "auth"`, exit 1, nothing at the canned provider. So a missing or misplaced
    providers file is not a refusal: it is a silent fallback to the vendor. The adapter must
    write `providers.json` under `CLINE_DATA_DIR/settings/` and nowhere else, and should treat
    any `model.provider` other than `openai-compatible` in `run_result` as a compile error.

Not measured, and so not claimed: whether a cwd-local `.cline/` MCP file is read (the bundle has
no `mcp.json` string; not probed at runtime); the `hub.drain`/`hub.status` protocol; ACP mode
(`--acp`); `--worktree`; what `--zen` does; any behaviour under a real Cline account.

Not yet consumed by any crate; `spikes/s27/run.py` reproduces every capture from a clean
scratch directory.
