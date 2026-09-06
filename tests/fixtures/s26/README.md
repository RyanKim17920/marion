# S26 — goose as a marion child, against a canned local provider, at $0.00

Measured 2026-09-05, macOS darwin 25.5.0, **goose 1.49.0** (`/opt/homebrew/bin/goose`, installed
with `brew install block-goose-cli`). Provider: a local OpenAI Chat Completions SSE stub on
`127.0.0.1` that answers the turn whose `tools[]` names `marion__report` with a streamed
`tool_calls` fragment set, the turn carrying a `role: "tool"` message with text, and everything
else (goose's title-generation side request) with a stub. MCP server: a stdio stub exposing one
tool, `report`, logging every frame. **No vendor endpoint, no `goose configure`, no login, no real
key. Total spend: $0.00.** Every run was bounded by `timeout 90`; none came near it.

The API key handed to every run was `marion-canned-credential-26bb`. goose never echoes it: it is
sent only as `Authorization` (36 chars, recorded by the stub as a length), and it is absent from
goose's own logs under `~/.local/state/goose/logs/` (grepped). It appears in no capture here
(`redact.py` asserts that on every file it writes).

Driver: `spikes/s26/run.py` (scenarios), `spikes/s26/canned_provider.py`,
`spikes/s26/mcp_report_server.py`, `spikes/s26/redact.py`.

## Files

| file | what it is |
|---|---|
| `goose-report.stdout.jsonl` | the run the adapter compiles: `--no-session --no-profile --with-builtin developer --with-extension "marion:S26_MCP_LOG=… python3 …/mcp_report_server.py"`, `GOOSE_MODE=auto`; four frames: `toolRequest` → `toolResponse` → text → `complete`, exit 0 |
| `goose-report.meta.json` | that run's exact argv, env (key redacted), cwd, exit code |
| `goose-report.provider-requests.jsonl` | everything the provider saw, in order with relative times: `GET /v1/models`, then three `POST /v1/chat/completions` — the title side request (no tools), the real turn (six tools), the after-tool turn |
| `goose-report.provider-request-1.json` | the real turn's request body — `stream: true`, `stream_options.include_usage`, `tools[]` naming exactly `edit`, `marion__report`, `read_image`, `shell`, `tree`, `write`, header `agent-session-id` (system prompt elided) |
| `goose-report.mcp.jsonl` | that run's MCP transcript: `initialize` (protocol `2025-11-25`, client `goose-cli 1.49.0`), `notifications/initialized`, `tools/list`, one `tools/call` naming the **bare** `report`, `_meta.agent-tool-call-request-id` = the provider's `call_s26_marion_1` |
| `goose-report-noquiet.stdout.txt` | the same run without `-q`: a three-line ASCII banner carrying the session id precedes the JSONL **on stdout** |
| `goose-report-iserror.stdout.jsonl` / `.mcp.jsonl` | the MCP stub answering `tools/call` with `isError: true`: `toolResponse.toolResult.status` stays `"success"`, `value.isError: true`, the model gets another turn, exit **0** |
| `goose-provider-500.stdout.jsonl` / `.provider-requests.jsonl` | the provider answering 500: the real turn retried four times over ~7 s (0.08, 1.11, 3.48, 7.23 s), then a plain `message` frame `"Ran into this error: Server error (500 Internal Server Error) at …"`, a `complete` frame with zero tokens, exit **0** |
| `goose-default-mode.stdout.jsonl` | `GOOSE_MODE` unset, empty config: the tool call runs without a prompt, exit 0 |
| `goose-chat-mode.stdout.jsonl` | `GOOSE_MODE=chat`: no `tools/call`; `toolResponse` carries `resultType: "complete"` and a canned "skipped in goose chat mode" text, `isError: false`, exit 0 |
| `goose-approve-mode.stdout.jsonl` / `.stderr.txt` | `GOOSE_MODE=approve` headless: the `toolRequest` frame, then stderr `Tool approval required in non-interactive mode … Use GooseMode::Auto`, no `complete` frame, exit **1** |
| `goose-builtins-default.provider-request-1.json` | no `--no-profile`, empty config dir: nineteen tools offered, `shell`/`edit`/`write` among them |
| `goose-with-builtin-developer.provider-request-1.json` | `--no-profile --with-builtin developer`: the six-tool list above — the developer tools are **unprefixed** |
| `goose-provider-paths.txt` | URL composition: default `OPENAI_BASE_PATH` is `v1/chat/completions`; a custom base path moves the models probe too; a host ending in `/v1` yields `/v1/v1/…` |
| `goose-session-first.stdout.jsonl` | `-n s26-sess`, no `--no-session`: byte-identical frame shapes to the no-session run — **no session-id frame exists** |
| `goose-session-first.mcp.jsonl` | the same extension process log across the first run and two resumes: each resume re-`initialize`s the stored extension |
| `goose-session-list.json` | `goose session list --format json`: the id (`20260906_35`), the name, `goose_mode`, and the extension **with its env values** persisted |
| `goose-session-resume-redeclare.stdout.txt` | `--resume -n s26-sess --with-extension marion:…`: `error: Failed to collect extensions: invalid config: extension name 'marion' is already in use` — printed to **stdout**, exit 1 |
| `goose-session-resume.stdout.jsonl` / `.stderr.txt` / `.provider-request-1.json` / `.meta.json` | `--resume -n s26-sess` without redeclaring: history replayed to the provider (six messages), a cwd-mismatch warning on stderr, exit 0; `.meta.json` holds all four session argvs |
| `goose-session-id-resume.stdout.jsonl` | `--resume --session-id 20260906_35`, same shape |
| `goose-config-dir-chat.stdout.jsonl` | `GOOSE_MODE: chat` in `$GOOSE_CONFIG_DIR/config.yaml`, env unset: the tool **runs** — the file is not read |
| `goose-xdg-config-chat.stdout.jsonl` | the same yaml in `$XDG_CONFIG_HOME/goose/config.yaml`: the tool is skipped — that file is read |
| `goose-info-paths.txt` | `goose info` under five env shapes: `GOOSE_CONFIG_DIR` changes nothing; `XDG_CONFIG_HOME` moves config only; `HOME` moves all three |
| `goose-state-dirs.json` | files created/modified during the run under the real `$HOME` vs a relocated one |
| `goose-env-inherit.mcp.jsonl` / `.stdout.jsonl` / `.meta.json` | `--with-extension "marion:python3 …/mcp_report_server.py <argv log>"` with `S26_MCP_LOG` and `S26_INHERITED=yes` set only in goose's own env: the child logged to the env path (no argv-path log was written) and saw `S26_INHERITED=yes` — goose's environment is inherited |
| `goose-models-plain.stdout.jsonl` / `.stderr.txt` / `.provider-requests.jsonl` / `.meta.json` | `GET /v1/models` answered `200 text/plain "marion canned provider\n"`: the run is byte-for-byte the happy path, empty stderr, exit 0 |

Redactions: run directories → `<RUN-DIR>` (or `<RUNS>/<scenario>` when a file spans scenarios),
the repository → `<REPO>`, `$HOME` → `<HOME>`, UUIDs → `<UUID>`, goose's `created` epoch →
`"<TS>"`, `<current-time>` → `<TS>`, loopback ports → `<PORT>`, provider request times → seconds
relative to the first request, every `role: "system"` body → elided with its length. Session ids
(`20260906_N`, UTC date plus a per-day counter) are left as goose minted them.

## What was learned, in the order it bit

1. **Env alone is enough; there is no configure step.** `GOOSE_PROVIDER=openai GOOSE_MODEL=…
   OPENAI_API_KEY=… OPENAI_HOST=http://127.0.0.1:PORT` with no `config.yaml` anywhere ran to
   completion on the first try. goose probes `GET {host}/{base_path minus "chat/completions"}/models`
   before the first turn (`/v1/models` by default); the stub answers with the configured model.
   A non-JSON 200 on that probe is ignored (item 14); a 404 or an empty list: not measured, and so
   not claimed. Likewise not measured: whether goose touches the macOS keychain when the key is in
   the environment, and `--with-streamable-http-extension`.
2. **The host is taken verbatim and `OPENAI_BASE_PATH` (default `v1/chat/completions`) is
   appended.** Pass the bare origin: a host ending in `/v1` produces `/v1/v1/chat/completions`.
3. **`--with-extension "marion:ENV=v cmd args"` is the declaration.** The `name:` prefix names the
   extension; `ENV=v` pairs before the command reach the child's environment (the stub recorded
   `S26_MCP_LOG`); argv is split on whitespace. The model sees `marion__report`
   (`<extension>__<tool>`), `tools/call` carries the bare `report`, and `_meta.agent-tool-call-
   request-id` on that call is the provider's own tool-call id.
4. **`--no-profile` is the only way to stop the default extensions.** With an empty config dir
   and no flag, goose still loads developer, apps, extensionmanager, todo and the skill/delegate
   tools — nineteen tools — and, on the first such run of the day, created
   `~/.local/share/goose/apps/clock.html` (seen in the state diff of an earlier pass, not in a
   file here; the file already existed when the captured pass ran).
   `--no-profile --with-builtin developer` gives exactly `edit read_image shell tree write` plus
   marion. Builtin developer tools are unprefixed; `apps__`, `todo__`, `extensionmanager__` are not.
5. **Stream grammar: two frame types.** `{"type":"message","message":{id, role, created,
   content[], metadata}}` and a final `{"type":"complete", total_tokens, …}`. The tool call is a
   `content[]` item `type: "toolRequest"` with `id` (the provider's call id), `toolCall.status`,
   `toolCall.value.name`, `toolCall.value.arguments` (parsed object), `_meta.goose_extension`. The
   result is a `role: "user"` message whose item is `type: "toolResponse"` with the same `id` and
   `toolResult.status` / `toolResult.value.{content[], isError}`. No delta frames.
6. **`toolResult.status` is not the MCP verdict.** An `isError: true` answer arrives as
   `status: "success", value.isError: true`; the model gets another turn; exit 0. Read `isError`.
7. **A provider fault is silent success.** Four retries in ~7 s, then the failure is delivered as
   an ordinary assistant `message` beginning `Ran into this error:` and a zero-token `complete`,
   exit **0**. The stream has no error frame type; the adapter must pattern-match that text or
   count the missing `toolRequest`.
8. **`-q` is required for a parseable stream.** Without it the ASCII banner (with the session id)
   is printed to stdout before the JSONL. Errors such as the extension-name clash are also plain
   text on stdout, exit 1.
9. **Headless approval is auto or nothing.** `GOOSE_MODE` unset behaves as auto (the call ran,
   exit 0). `approve` aborts the run with exit 1 after the `toolRequest` frame and no `complete`.
   `chat` withholds every call and feeds the model a canned "skipped" tool result. Setting
   `GOOSE_MODE=auto` explicitly is the safe compile.
10. **There is no session-id frame.** Not with `--no-session`, not with `-n`. The id is visible in
    the banner (suppressed by `-q`), in the `agent-session-id` HTTP header the provider receives,
    in MCP `_meta.agent-session-id`, and in `goose session list --format json` (`id`, `name`).
    Resume is `--resume -n <name>` or `--resume --session-id <id>`; `-s` is `--interactive`.
11. **Resume restores the extension from the store — do not redeclare it.** The session row
    persists the stdio command *and its env values*; a second `--with-extension marion:` fails
    with `extension name 'marion' is already in use`, exit 1. On resume the stored extension is
    relaunched (fresh `initialize` + `tools/list`) and the full history is replayed to the provider.
    A cwd that differs from the stored one only warns on stderr.
12. **`GOOSE_CONFIG_DIR` does nothing in 1.49.0.** `goose info` reports `~/.config/goose` with
    it set, and a `config.yaml` placed there is ignored. `XDG_CONFIG_HOME` relocates config only;
    `HOME` relocates config, the sqlite session store (`.local/share/goose/sessions/sessions.db`)
    and logs (`.local/state/goose/logs/`, including `llm_request.N.jsonl` full request dumps);
    `XDG_DATA_HOME`/`XDG_STATE_HOME` split those out. Even `--no-session` runs touch the store's
    WAL and write the log dumps, so a sandboxed `HOME` is the isolation.
13. **The stdio extension child inherits goose's whole environment.** With `S26_MCP_LOG` and
    `S26_INHERITED=yes` set only on the goose process and a declaration carrying no `ENV=v`, the
    stub logged to the env path and recorded `S26_INHERITED=yes`; the argv fallback log was never
    created. `ENV=v` in the declaration is additive, not the only channel — and, per item 11, the
    only channel the session store persists. Anything marion puts in goose's env reaches the bridge.
14. **The `/models` probe tolerates a non-JSON 200.** A `text/plain` body produced a run
    byte-identical to the happy path, empty stderr, exit 0. The probe's result is not checked
    against `GOOSE_MODEL` when the body is not a model list.

Consumed by a future `crates/marion-harness/src/goose.rs`; every claim above that is a claim about
bytes is in one of these files.
