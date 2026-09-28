# s36 — opencode parity probes (opencode 1.18.32, 2026-09-27)

Measurements taken to bring the three opencode shapes marion drives — `opencode run` (headless),
`opencode acp` (ACP) and `marion opencode` (native TUI) — level with the claude and codex rows.
Every probe ran against a local OpenAI-compatible stub standing in for a provider (no model, no
login, $0), in a throwaway `HOME`/`XDG_*` sandbox with opencode's own config document naming the
stub — the same isolation marion's canned rows compile. Each probe is one transcript and one
verdict: **PASS** (the harness did what marion needs), **FAIL** (it did not, and the row now
compensates or the gap is named), or **UNSUPPORTED**.

Host: macOS 26.5.1 (arm64, Darwin 25.5.0), load average 50–110 from other agents' runs.

## `run-usage.stdout.jsonl` — `step_finish` token accounting

`opencode run --pure --format json --title t -m canned/canned-1 "say hi"`, stdin closed. The stub
answered one text turn and a final usage chunk:
`{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050,
"prompt_tokens_details":{"cached_tokens":300},"completion_tokens_details":{"reasoning_tokens":7}}`.
opencode sent `stream_options: {"include_usage": true}`.

The `step_finish` it printed: `tokens: {total: 1050, input: 700, output: 43, reasoning: 7,
cache: {write: 0, read: 300}}`. So `input` excludes the cache reads, and `output` **excludes the
reasoning tokens** — unlike codex's and claude's counters. **FAIL for marion before this fixture**
(the row read `output` 43 and lost 7 generated tokens); the row now adds `tokens.reasoning` into
output, and the four counters sum to opencode's own `total`
(`token_usage::opencodes_measured_step_finish_counts_reasoning_as_output_and_sums_to_its_own_total`).

`opencode acp` reports the same split on the `session/prompt` response
(`inputTokens` 700, `outputTokens` 43, `thoughtTokens` 7, `cachedReadTokens` 300,
`totalTokens` 1050), while `codex-acp` (s22) counts its thoughts **inside** `outputTokens`. Each
ACP row now states its split (`acp::Agent::reasoning`, beside for opencode, within for codex-acp),
so both read `output` as every generated token
(`token_usage::an_acp_agents_thought_tokens_are_counted_once_as_output_and_stated_as_reasoning`).

Redaction: the session id is the stable fake `ses_s36fake0000000000000000001` (same length as the
real one). Part and message ids are per-run and kept.

## `mcp-ready-run-*`, `mcp-call-*` — does `opencode run` wait for marion's MCP server?

`probes/slowmcp.py` is a stdio MCP server that holds its `initialize` answer (`SLOW_DELAY`) or a
`tools/call` answer (`SLOW_CALL`) and logs when it did (`mcp-server.jsonl`); `probes/prov.py` logs
each model request's time and tool names (`provider-requests.summary.jsonl`, bodies dropped) and,
given a tool name, calls it once. Each directory has the run's `stdout.jsonl` and a `verdict.txt`.

| probe | server | `mcp.slow.timeout` | verdict |
|---|---|---|---|
| `mcp-ready-run-8s` | `initialize` held 8 s | unset | **PASS** — first request after the answer, carrying `slow_report` |
| `mcp-ready-run-45s` | `initialize` held 45 s | unset | **FAIL** — first request at ~34 s after the server started, **without** its tool, no warning on stdout |
| `mcp-ready-run-45s-timeout` | `initialize` held 45 s | 90000 | **PASS** — waited, request carried the tool |
| `mcp-call-330s` | `tools/call` held 330 s | unset | **FAIL** — the call failed at 60.0 s: `MCP error -32001: Request timed out` |
| `mcp-call-90s-timeout` | `tools/call` held 90 s | 600000 | **PASS** |
| `mcp-call-65s-day-timeout` | `initialize` 3 s, `tools/call` 65 s | 86400000 | **PASS** — a one-day value does not overflow |

So `opencode run` does wait for its MCP servers before the first model request (unlike codex's
app-server, which sends without tools after ~1 s), but only for ~30 s, and it abandons any call
longer than 60 s. marion's `spawn` (foreground) and `wait` block for a child's whole run, so the
row now gives marion's server `"timeout": 86400000` (`opencode::MCP_TIMEOUT_MS`), on the canned
document, the live inline document and the native injection alike.

Running a probe: `probes/mkenv.sh` expects `probes/sb-warm/`, a sandbox in which one
`opencode run` has already completed (the first run in a fresh `HOME` installs
`@opencode-ai/plugin` into the config dir, which took minutes under load). Close stdin
(`</dev/null`): with a pipe on stdin, `opencode run` waits for it to reach EOF before it starts.

## `mcp-acp-*` — the same questions over `opencode acp`

`probes/drive_acp.py` is a minimal ACP client (`initialize`, `session/new` declaring
`probes/slowmcp.py` in `mcpServers`, one `session/prompt`, `allow_once` on any permission ask);
`acp-client.jsonl` is every frame both ways with its time.

| probe | server | config | verdict |
|---|---|---|---|
| `mcp-acp-ready-8s` | `initialize` held 8 s | — | **PASS** — `session/new` answered only after the server was up |
| `mcp-acp-ready-45s` | `initialize` held 45 s | — | **FAIL** — `session/new` answered at ~33 s and the session ran without the tool |
| `mcp-acp-ready-45s-experimental` | `initialize` held 45 s | `experimental.mcp_timeout` 86400000 | **FAIL** — the startup limit is not that key |
| `mcp-acp-call-75s` | `tools/call` held 75 s | — | **FAIL** — `tool_call_update` `failed` at 60 s, `MCP error -32001: Request timed out` |
| `mcp-acp-call-75s-experimental` | `tools/call` held 75 s | `experimental.mcp_timeout` 86400000 | **PASS** |

ACP's `mcpServers` entry has no timeout field, so a `session/new` server takes opencode's
fallback, `experimental.mcp_timeout`. marion now sets it on every `opencode acp` node through
`OPENCODE_CONFIG_CONTENT` (`opencode::acp_session_document`), under both modes. The ~30 s startup
limit remains: marion's bridge must answer `initialize` inside it, which it does in well under a
second when the machine is not saturated.

## `model-omitted/` — what `opencode run` does with no `-m`

The sandbox config names `"model": "canned/canned-1"`; the run passed no `-m`. The request went
out for `canned-1`: **PASS**. So a live node that names no model of its own can leave `-m` off and
run on the operator's configured default, as a live claude or codex node does. marion used to
refuse that launch (the `opencode` type's default, `marion/default`, names the provider block only
a canned node generates); it now treats the plumbing default as naming no model under `--live`,
for `opencode run` and for an `opencode acp` session alike (`opencode::requested_model`).

## `held-first/` — when does `opencode run` name its session?

The stub held the first model request for 20 s. Three seconds into the hold `stdout.jsonl` had
**no frame at all**; the three frames (`step_start`, `text`, `step_finish`, each carrying
`sessionID`) arrived only once the response streamed. **FAIL** against claude and codex, which
name their session (`system/init`, `thread.started`) before their first request: an opencode node
whose supervisor dies while its first request is in flight has journaled no session, so
`marion resume` has nothing to hand back and refuses. `restart_resume`'s opencode arc therefore
parks the child on its second request, after the first response named the session, and from there
the resume is measured working (`a_lost_opencode_child_resumes_its_own_session_under_its_parent_
and_takes_its_next_turn`). A way past the gap, not yet taken: marion titles every opencode session
`marion-<agent id>`, so the id could be looked up by title in the node's own `OPENCODE_DB`.

## `permission-*` — an operator's `"ask"` on a tool marion's node calls

| probe | shape | verdict |
|---|---|---|
| `permission-run-ask` | `opencode run`, config `permission: {"slow_report": "ask"}` | **FAIL** — `auto-rejecting` on stderr, the tool part `error` *The user rejected permission…*, exit 0 |
| `permission-run-ask-inline-allow` | the same, with `OPENCODE_CONFIG_CONTENT={"permission":{"slow_*":"allow"}}` | **PASS** — the inline allow, merged over the file, wins |
| `permission-acp-ask` | `opencode acp`, the same config | **PASS** — `session/request_permission` with `allow_once`/`allow_always`/`reject_once`; answered `once`, the call completed |

So a headless `opencode run` node whose operator configured `"ask"` for marion's tools had them
silently refused (the row's grammar fails a refused `report`, so the run was not a false success,
but a root's `spawn` was lost). marion's declaration now carries `permission: {"marion_*":
"allow"}` beside its `mcp` block on every route, the way codex's and gemini's declarations approve
their server's tools. Over ACP the same ask reaches marion, which answers `allow_once` by kind, as
it does for every agent.
