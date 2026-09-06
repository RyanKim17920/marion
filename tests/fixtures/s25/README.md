# S25 — Qwen Code as a marion child, against a canned local provider, at $0.00

Measured 2026-09-05, macOS darwin 25.5.0, **qwen 0.23.0** (`/opt/homebrew/bin/qwen`, installed with
`npm i -g @qwen-code/qwen-code@0.23.0`; a Gemini CLI fork whose headless surface has since diverged
into a Claude-Code-shaped one — see item 2). Provider: a local OpenAI Chat Completions SSE stub on
`127.0.0.1` that answers each turn by request *shape* (system-prompt prefix, `tools[]`, and which
tools the history already called), never by a counter. MCP server: a stdio stub exposing one tool,
`report`. **No vendor endpoint, no Qwen OAuth, no real key. Total spend: $0.00.**

The API key handed to every run was `marion-canned-credential-25aa`. It appears in no file here; the
provider logs the `Authorization` header as its length only (36 chars: `Bearer ` + 29), and
`redact.py` refuses to write a fixture that contains it.

Driver: `spikes/s25/run.py` (one scenario per run directory), stubs `spikes/s25/canned_provider.py`
and `spikes/s25/mcp_report_server.py`, redaction `spikes/s25/redact.py`.

## Files

| file | what it is |
|---|---|
| `qwen-write-then-report.stdout.jsonl` | the run the adapter compiles: `--yolo --core-tools write_file mcp__marion__report --exclude-tools <12 survivors>`, `QWEN_CODE_LEGACY_MCP_BLOCKING=1`, memory side-turn off; `write_file` lands a file, then `mcp__marion__report` is answered `recorded`, exit 0 |
| `qwen-write-then-report.provider-request-1.json` | that run's first request as the provider saw it — `POST /v1/chat/completions` against a `…/v1` base URL, `stream: true`, `tools[]` naming exactly `mcp__marion__report` and `write_file` (system prompt and startup reminders elided to their lengths) |
| `qwen-write-then-report.mcp.jsonl` | that run's MCP transcript: `initialize` (protocol `2025-11-25`, client `qwen-cli-mcp-client-marion`), `notifications/initialized`, `prompts/list`, `resources/list`, `tools/list`, one `tools/call` naming the **bare** `report` |
| `qwen-write-then-report.settings.json` | the `$QWEN_HOME/settings.json` that run used — `mcpServers.marion.{command,args,env}` and `memory.enableManagedAutoMemory: false` — **after qwen rewrote it**, appending `"$version": 4` |
| `qwen-write-then-report.argv.json`, `.process-env.json`, `.stderr.txt` | the exact argv, the exact (key-elided) environment, and the one stderr line (`--yolo` warning) |
| `qwen-baseline-deferred-mcp.stdout.jsonl` | `--yolo` and nothing else: the MCP tool is **deferred** — the canned model has to call `tool_search` (`select:mcp__marion__report`, answered `Loaded 1 tool(s)`) before the report call is possible |
| `qwen-baseline-deferred-mcp.provider-request-1.json` | that run's first request: 28 built-in tools, no `mcp__marion__report`, and the `<system-reminder>` that says the MCP tool "became available after startup and [is] reachable via `tool_search`" |
| `qwen-blocking-mcp.provider-request-1.json` | same settings plus `QWEN_CODE_LEGACY_MCP_BLOCKING=1`: `mcp__marion__report` is in `tools[]` on request one |
| `qwen-visible-tools.provider-request-1.json`, `qwen-visible-tools.settings.json` | the settings-only alternative, `tools.visible: ["mcp__marion__report"]`, no env var: also in `tools[]` on request one |
| `qwen-core-tools-alone.provider-request-1.json` | `--core-tools write_file mcp__marion__report` without `--exclude-tools`: fourteen tools, the twelve survivors being `agent enter_worktree exit_worktree get_goal list_agents record_artifact report_findings send_message skill task_stop tool_search update_goal` |
| `qwen-bare-mcp-config.stdout.jsonl` | `--bare --mcp-config '{"mcpServers":…}'`: `--core-tools` is ignored, the fixed bare set `read_file edit notebook_edit run_shell_command get_goal update_goal` plus the MCP tool is offered, and nothing is written under `QWEN_HOME` |
| `qwen-mcp-config-argv.{stdout.jsonl,provider-request-1.json,mcp.jsonl,argv.json,home-after.txt,settings.json}` | the compiled run with the server declared **only** on argv, `--mcp-config '{"mcpServers":{"marion":{command,args,env}}}'`, no `--bare`, no `mcpServers` in any settings file (settings carries only the memory switch): `mcp__marion__report` is in `tools[]` on request one, `tools/call` reaches the stub, exit 0; `QWEN_HOME` still receives the session, usage and `installation_id` files (listed) |
| `qwen-report-iserror.stdout.jsonl` | the MCP stub answering `tools/call` with `isError: true`: the `user` frame's `tool_result` has `is_error: true` and a content string that embeds the MCP response; `result.subtype: "success"`, exit **0** |
| `qwen-denied-without-yolo.stdout.jsonl` | no `--yolo`: `permission_mode: "auto"`, the report call is declined (`is_error: true`, "non-interactive mode cannot prompt for confirmation"), `result.permission_denials[]` names it, exit **0** |
| `qwen-denied-without-yolo.provider-request-2.json` | the side request `auto` mode made *to the provider* to decide that call: a classifier prompt offered one tool, `respond_in_schema` |
| `qwen-provider-500.stdout.jsonl`, `qwen-provider-500.provider-timeline.jsonl` | the provider answering 500: **28 requests over 86 s** (seven outer attempts of four inner tries, backoff growing to 27 s), then an `assistant` text `[API Error: 500 canned failure]` and `result.subtype: "success", is_error: false`, exit **0** |
| `qwen-provider-500-wall-time.stdout.jsonl`, `.stderr.txt` | the same under `--max-wall-time 30`: no `result` frame, stderr `Run aborted: wall-clock budget of 30s exceeded`, exit **55** |
| `qwen-no-auth.stdout.jsonl` | no `OPENAI_*` and no settings: one `result` frame, `subtype: "error_during_execution"`, `error.message: "No auth type is selected…"`, exit 1 |
| `qwen-settings-auth.settings.json`, `qwen-settings-auth.stdout.jsonl` | the provider declared in settings instead of env: `security.auth.{selectedType: "openai", apiKey, baseUrl}` + `model.name` (key elided), exit 0 |
| `qwen-env-only.home-after.txt` | everything qwen wrote under `QWEN_HOME` for one env-only run: `settings`-less, but `projects/<cwd-slug>/chats/<session>.jsonl`, `usage/`, `installation_id`, `memories/` |
| `qwen-memory-side-turn.provider-request-2.json` | the hidden second request an env-only run makes after the answer: a "managed memory extraction subagent" prompt offered `read_file grep_search glob run_shell_command write_file edit` |
| `qwen-project-settings.stdout.jsonl`, `.work-after.txt` | `mcpServers` declared in the cwd's `.qwen/settings.json` instead of `QWEN_HOME`: `mcp_servers: [{marion, connected}]`, same run shape |
| `qwen-resume-turn-1.stdout.jsonl`, `qwen-resume-turn-2.stdout.jsonl`, `.argv.json`, `.provider-request-1.json` | turn one, then `--resume <session_id>` with the id read off turn one's `init` frame: turn two's `init` carries the **same** `session_id`, and its first request replays turn one's history (`assistant` with `tool_calls`, `tool` result, `assistant` text) before the new prompt |

Redactions: run directories → `<RUN-DIR>` (and their slug form in `projects/`), the checkout →
`<REPO>`, the interpreter → `<PYTHON>`, `$HOME` → `<HOME>`, UUIDs → `<UUID-N>`, the key → `<KEY>`
(nowhere), the `init` frame's `slash_commands`/`agents` arrays → entry counts (they list the
operator's own skills, item 11), the system prompt → its length, every startup `<system-reminder>`
except the MCP-tools one → its length, and in requests with more than ten tools the built-in tools'
descriptions/parameters → their lengths. `tools[]` names are verbatim everywhere.

## What was learned, in the order it bit

1. **Env alone is enough; no login.** `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_MODEL` and
   `qwen -p "…" --output-format stream-json` run to exit 0 with `permission_mode: "auto"`. Without
   them the CLI exits 1 with a single `result` frame, `error_during_execution`, `No auth type is
   selected`. The base URL is taken verbatim in marion's `…/v1` form and `/chat/completions` is
   appended; the request carries `stream: true`, `stream_options.include_usage`, `max_tokens: 32000`,
   `User-Agent: QwenCode/0.23.0`. The settings route is `security.auth.{selectedType,apiKey,baseUrl}`
   (key in plaintext on disk) — a wrong guess (`openai.apiKey`) fails with the message that names the
   right key.
2. **The stream is Claude Code's, not Gemini CLI's.** `system`/`init` (`session_id`, `tools[]`,
   `mcp_servers[]`, `permission_mode`, `model`), `stream_event` (`goal_state`), `assistant` whose
   `message.content[]` carries `tool_use {id, name, input}` with `input` already parsed, `user` whose
   `message.content[]` carries `tool_result {tool_use_id, is_error, content}`, and `result`
   (`subtype`, `is_error`, `num_turns`, `result`, `usage`, `permission_denials[]`). Pairing is
   `tool_use.id` ↔ `tool_result.tool_use_id`; the id is the provider's `call_…` id passed through.
   Every frame repeats `session_id`. `gemini.rs`'s `tool_use`/`tool_result`/`init` top-level types
   do not apply.
3. **MCP tools are deferred behind `tool_search` by default.** Discovery runs in the background
   (`Config.initialize`, `startMcpDiscoveryInBackground`), the first declaration list is built
   without the server's tools, and anything that arrives later is announced in a reminder and made
   reachable only through `tool_search` for the whole session — the `init` frame lists
   `mcp__marion__report` while `tools[]` does not. Two measured fixes: `QWEN_CODE_LEGACY_MCP_BLOCKING=1`
   (discovery blocks startup; the tool is first in `tools[]`) or `settings.tools.visible:
   ["mcp__marion__report"]` (a deferred tool shown from session start). The adapter should set the
   env var: `visible` alone still races discovery per the source comment "tools from servers that
   connect later stay deferred until the next session start".
4. **Two spellings.** The model sees `mcp__<server>__<tool>` — `mcp__marion__report` — and that is
   what `--core-tools`, `--exclude-tools`, `tools.visible` and `permission_denials[].tool_name`
   use. `tools/call` names the bare `report`. The handshake also issues `prompts/list` and
   `resources/list`; a server that answers `-32601` to both is fine.
5. **Three axes of tool restriction, and only one combination reaches two names.** Default `--yolo`
   offers 28 built-ins (`agent cron_create cron_delete cron_list edit enter_worktree exit_worktree
   get_goal glob grep_search list_agents loop_wakeup monitor notebook_edit read_file
   read_mcp_resource record_artifact report_findings run_shell_command send_message skill task_stop
   todo_write tool_search update_goal web_fetch write_file zoom_image`). `--core-tools` is a legacy
   allowlist with twelve exempt survivors (`qwen-core-tools-alone…`); `--exclude-tools` naming those
   twelve on top yields exactly `mcp__marion__report, write_file`. `--bare` ignores `--core-tools`
   and offers its own six (incl. `run_shell_command`, `edit`; no `write_file`).
6. **Without `--yolo`, headless mode asks a model.** `permission_mode: "auto"` sends the provider a
   classifier request (one tool, `respond_in_schema`) per tool call; when it is not answered in
   schema the call is declined with `is_error: true` and the run still exits **0**, the denial
   recorded in `result.permission_denials[]`. That is §12's silent-success shape and a second
   billable request per call. `--yolo` (`permission_mode: "yolo"`) runs everything and prints one
   stderr warning, silenced by `QWEN_CODE_SUPPRESS_YOLO_WARNING=1`.
7. **An MCP `isError` is a `tool_result` with `is_error: true`, exit 0.** The content is a string
   embedding the MCP response (`refused: not authorized` is inside it); `result.subtype` stays
   `success`. Same shape as a denial; only the text differs.
8. **A dead provider is retried 28 times and then reported as success.** Seven outer attempts
   (`DEFAULT_MAX_ATTEMPTS = 7`) of four inner tries (`DEFAULT_MAX_RETRIES = 3`), backoff to 27 s,
   86 s total, then `assistant` text `[API Error: 500 canned failure]` and `result.is_error: false`,
   exit 0. The only bound qwen offers is its own `--max-wall-time N`: exit **55**, no `result` frame,
   one stderr line. The adapter needs both a wall clock and a rule that an `assistant` text starting
   `[API Error:` is a failure.
9. **There are hidden side turns that get tools.** After the answer, an env-only run made a second
   request whose system prompt opens `You are now acting as the managed memory extraction subagent`
   and whose `tools[]` includes `write_file`, `edit`, `run_shell_command`. A provider that dispatches
   positionally would hand it the scripted call. `memory.enableManagedAutoMemory: false` removes it;
   the canned provider additionally dispatches on the main system prompt's prefix `You are Qwen Code`.
10. **`QWEN_HOME` relocates settings, sessions (`projects/<cwd-slug>/chats/<id>.jsonl`), usage,
    `installation_id` and memories; `~/.qwen` kept its mtime across every run.** But `qwen --help`
    with no `QWEN_HOME` created `~/.qwen` on its own, and qwen rewrites the settings document it is
    given (`"$version": 4` appended).
11. **`HOME` still leaks in.** `SKILL_PROVIDER_CONFIG_DIRS = [".qwen", ".agents"]`: the operator's
    `~/.agents/skills` (33 here) are listed in the prompt and in `init.slash_commands`. `--bare` skips
    that discovery — at the cost of item 5.
12. **A project `.qwen/settings.json` in the cwd is honoured** for `mcpServers`, same shape.
13. **`--resume <id>` exists and works, keyed by cwd.** The id is `init.session_id`; turn two must
    run under the same `QWEN_HOME` *and* the same cwd, because the session file lives under the
    cwd's slug — a copied tree under another path gets `No saved session found with ID …`, exit 1.
    Turn two's `init` repeats the same `session_id`, and its first request replays the history.
    `--session-id` starts a session with a chosen id; `--chat-recording false` would disable both.
14. **`qwen` is a launcher.** `cli-entry.js` `spawnSync`s `node --expose-gc cli.js …`; killing the
    launcher PID leaves the child running to completion (the first provider-500 run kept writing
    frames after its 90 s kill). Kill the process group.
15. **`--mcp-config` on argv replaces the settings route.** The same `{"mcpServers":{…}}` document
    as an inline JSON string, no `--bare`, no settings `mcpServers`: identical run shape to
    `qwen-write-then-report` (tool first-class on request one under the blocking env var, `tools/call`
    reached, exit 0). The help text says the flag takes "a path to a JSON file or inline JSON" — a
    file path, not an `@path` form; the file-path form was not probed and is not claimed. Unlike
    `--bare`, `QWEN_HOME` is still written to (session, usage, `installation_id`), and the
    settings file that carried only the memory switch was still rewritten with `"$version"`.
16. `-p` is marked deprecated in favour of a positional prompt; it still works in 0.23.0 and is what
    every capture here used.

Consumed by nothing yet: this fixture set exists so `crates/marion-harness/src/qwen.rs` can be written
from these bytes.
