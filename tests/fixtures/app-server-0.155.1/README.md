# S36 — `codex app-server` 0.155.1 over stdio, measured for marion's planned transport

Measured 2026-09-27, macOS darwin 25.5.0, codex-cli 0.155.1 (the admitted pin), **$0.00 metered**.
Every model request went to marion's `canned` provider (built from this tree) behind a loopback
proxy that logs each request, can hold one, and answers probe-specific steps itself (a shell call,
`sleep`, an escalation). Each run used a scratch `CODEX_HOME` whose `config.toml` is what
`codex::config_toml` renders under canned (`model_provider = "canned"`, `approval_policy = "never"`,
`sandbox_mode = "workspace-write"`, `features.plugins = false`), with marion's real bridge
(`marion-supervisor mcp`) as `[mcp_servers.marion]` unless a probe declared it another way. No
login, no operator `~/.codex`. The driver (a Python JSON-RPC stdio client logging both directions)
and the untrimmed logs stayed in the session scratchpad.

## Format

One JSON object per line: `{"t": seconds, "dir": …, "msg": …, "src": probe}`.

| `dir` | meaning |
|---|---|
| `c2s` | a frame the client wrote to app-server's stdin |
| `s2c` | a frame app-server wrote to stdout (every stdout line in every run was JSON) |
| `prov` | a provider request as the proxy saw it (`req`: index, `input_tail`: last input items, `user_texts`: every user message but the environment context) or its answer (`resp`, `src`: `canned` or `proxy-script`) |
| `note` | the driver's one-line conclusion for a case — read these first |

Trimmed: strings over 320 chars are cut (`…[N more chars]`); `account/rateLimits/updated`,
`turn/diff/updated`, `remoteControl/status/changed`, `thread/started` and (outside p4)
`thread/tokenUsage/updated` are kept once per file; after the first, a `thread/start|resume`
result keeps only the thread id, turn count, sandbox and approval policy; outside p6 the
repetitive `userMessage` items, `thread/status/changed`, `turn/started` and (outside p3)
`mcpServer/startupStatus/updated` are capped. Paths are scrubbed to `<SCRATCH>`, `<WORKTREE>`,
`<HOME>` (REVIEW.md §3); the host name and installation id are `<HOST>` and `<INSTALLATION_ID>`.
Thread, turn and item ids and pids are kept: they are per-run, came from a canned run, and the
probes correlate by them (p7's late `item/completed`, p9's descendants).

## Files

| file | probe |
|---|---|
| `schema-subset.json` | P1: `generate-json-schema --out` (no `--experimental`) cut to the methods marion will use, listed in its `x-marion-methods`, plus the closure of their `$ref`s |
| `p2-handshake.jsonl` | P2: requests before `initialize` / before `initialized`, double `initialize`, `experimentalApi` gating, unknown method, missing `params`, stdin EOF |
| `p3-mcp-and-sandbox.jsonl` | P3: marion's server declared in `config.toml` / argv `-c` / `thread/start.config` (dotted and nested) / nowhere; a missing bridge; the sandbox and approval matrix |
| `p3-mcp-readiness.jsonl` | P3: a bridge behind a startup delay — what `thread/start`, `turn/start` and the first provider request wait for; `mcpServerStatus/list`; `omit_tools_from` absent |
| `p4-items.jsonl` | P4: one turn with an MCP `report` (direct and from code-mode JS), a shell call, `apply_patch`, a message; `thread/tokenUsage/updated` |
| `p4-canned-default.jsonl` | P4: the canned provider's own codex script (apply_patch → `report` → message) answered unmodified; its responses carry no `usage` |
| `p5-approvals.jsonl`, `p5-approvals.stderr.txt` | P5: `approvalPolicy: on-request` — escalated shell, patch outside the workspace, sandboxed write, network; each answered accept / decline / cancel / JSON-RPC error |
| `p5-mcp-approvals.jsonl` | P5: `default_tools_approval_mode = "prompt"` — the same four answers, and `approvalPolicy: never` |
| `p6-steer.jsonl` | P6: `turn/steer` mid tool call, while a provider request is held (first and last), wrong/missing `expectedTurnId`, unknown thread, after completion, during a pending approval, twice; `turn/start` during an active turn |
| `p7-interrupt.jsonl` | P7: `turn/interrupt` mid `sleep`, and while a provider request is held; re-interrupt; the next turn |
| `p8-resume.jsonl` | P8: persistent thread, SIGKILL, relaunch, `thread/resume`; resume overrides; ephemeral resume; `codex exec` thread resumed by app-server and the reverse |
| `p9-lifecycle.jsonl` | P9: stdin EOF idle and mid-turn, SIGTERM mid-turn; descendants; plugin fetch |
| `p10-optout.jsonl` | P10: `capabilities.optOutNotificationMethods` |

## What each probe found

**P1 schema.** 102 client methods without `--experimental`, 164 with. `turn/steer`,
`turn/interrupt`, `thread/resume`, `mcpServerStatus/list` are **not** experimental; `thread/queue/*`
is (the `thread/queue/changed` notification is in the stable union). `turn/steer` params:
`{threadId, input, expectedTurnId (required), clientUserMessageId?}` → `{turnId}`.
`turn/interrupt`: `{threadId, turnId}` → `{}`. `InitializeCapabilities`: `experimentalApi`,
`requestAttestation`, `optOutNotificationMethods?`, `extensions?`. v2 approval decisions:
command `accept | acceptForSession | {acceptWithExecpolicyAmendment} | {applyNetworkPolicyAmendment}
| decline | cancel`; file change `accept | acceptForSession | decline | cancel`; MCP elicitation
`{action: accept|decline|cancel, content, _meta}`. The legacy `applyPatchApproval` /
`execCommandApproval` take `ReviewDecision` (`approved | approved_for_session | {denied} | abort |
…`) and were never sent to a v2 thread in any run. `ThreadTokenUsage` = `{total, last,
modelContextWindow}`; `McpServerStartupState` = `starting | ready | failed | cancelled`.

**P2 handshake.** Frames carry no `"jsonrpc"` member either way, and notifications carry a
top-level `emittedAtMs` beside `method`/`params`. Before `initialize`: `-32600 "Not initialized"`.
A second `initialize`: `-32600 "Already initialized"`. `thread/start` **before** the `initialized`
notification succeeds. An experimental method without `experimentalApi`: `-32600 "<method>
requires experimentalApi capability"`. Unknown method and missing `params` are `-32600 "Invalid
request: …"`. No non-JSON stdout; stderr is empty except ANSI-coloured tracing `ERROR` lines when a
tool call fails (`p5-approvals.stderr.txt`). `userAgent` embeds `clientInfo.name`/`version` and the
parent terminal (`ghostty/1.3.1`).

**P3 declaration, readiness, sandbox.** marion's server starts when declared in `config.toml`,
by argv `-c mcp_servers.marion.*`, or by `thread/start.config` with dotted keys or a nested
`mcp_servers` object — per thread, each emitting `mcpServer/startupStatus/updated`
`starting → ready` (`failed` with `error: "MCP client for \`marion\` failed to start: …"` for a
missing binary; the turn then runs without marion's tools). **Nothing waits for readiness:**
`thread/start` returns in ~20 ms and `turn/start` at once; the turn's first provider request waits
at most ~1 s for a starting server, then goes out **without** marion's tools (bridge delayed
1.5 s / 2.5 s / 4 s: request at +1.02 s, tools absent). The gate is `mcpServer/startupStatus/updated`
`ready` for the thread (or `mcpServerStatus/list`, which **blocks** until startup finishes: 3.05 s
for a 3 s bridge). Without `omit_tools_from = ["deferred"]` the default model's (`gpt-6-astra`,
code mode) `exec` declaration lists no `mcp__marion__*`; with it all six verbs are listed.
Sandbox: `thread/start.sandbox` beats `config.toml` and argv `-c sandbox_mode` (argv
`danger-full-access` + param `read-only` → `readOnly`); `config.sandbox_mode` works too; the
response echoes `sandbox` as a `SandboxPolicy` (`workspaceWrite{writableRoots, networkAccess:false,
…}`, `readOnly`, `dangerFullAccess`) and `approvalPolicy`.

**P4 items.** `item/started` + `item/completed` per item: `userMessage`; `mcpToolCall`
(`server`, `tool`, `arguments`, `result.content`, `error`, `durationMs`; id = the provider call id
for a direct call, `exec-<uuid>` from code-mode JS); `commandExecution` (`command` as
`/bin/zsh -lc '…'`, `cwd`, `processId`, `source: unifiedExecStartup`, `aggregatedOutput`,
`exitCode`, `durationMs`); `fileChange` (`changes[{path, kind, diff}]`); `agentMessage` (`text`).
`thread/tokenUsage/updated` follows each provider response: `total` is cumulative for the thread,
`last` is that response's usage. `turn/completed` carries **no usage**, and its `turn.items` holds
only the final `agentMessage` (`itemsView: "summary"`). A provider response without `usage` emits
no token notification at all (`p4-canned-default.jsonl`).

**P5 approvals (`approvalPolicy: on-request`).** An escalated shell
(`sandbox_permissions: "require_escalated"`) sends `item/commandExecution/requestApproval`
`{threadId, turnId, itemId, kind: "command", reason (the justification), command, cwd,
commandActions, proposedExecpolicyAmendment, availableDecisions}` — `availableDecisions` lists
`accept`, `acceptWithExecpolicyAmendment`, `cancel`, yet `decline` is honoured. A patch outside the
workspace sends `item/fileChange/requestApproval` `{threadId, turnId, itemId, reason: null,
grantRoot: null}`. A plain sandboxed write outside the workspace and a network call are **not**
asked about: they fail inside the sandbox (`Operation not permitted`, `Could not resolve host`).
Answers → item status / next `function_call_output` text: accept → `completed`, real output;
decline → `declined`, `exec_command failed: … Rejected("rejected by user")` / `patch rejected by
user`, turn continues; **cancel → turn `interrupted`**, no further provider request; JSON-RPC
error → `failed` (command) or `declined` (patch), `approval request failed`, turn continues. Every
answer is followed by `serverRequest/resolved {threadId, requestId}`. MCP with
`default_tools_approval_mode = "prompt"` asks through `mcpServer/elicitation/request` `{serverName,
mode: "form", message: "Allow the marion MCP server to run tool \"report\"?", requestedSchema: {},
_meta: {codex_approval_kind: "mcp_tool_call", tool_params, …}}`; accept → `completed`, decline or
error → `failed` "user rejected MCP tool call", cancel → `failed` "user cancelled MCP tool call"
(the turn continues). Under `approvalPolicy: never` the same call fails visibly: "MCP tool call
requires approval, but approval policy is never".

**P6 steer.** `turn/steer` on an active turn answers `{turnId}` at once — mid tool call, while a
provider request is held, during a pending approval, and twice in a row. The text arrives as a
plain `role: "user"` message after the tool output in the **next provider request of the same
turn**, surfaces as a `userMessage` item, and there is **no new `turn/started` and exactly one
`turn/completed`**. It never cuts a running tool short (`sleep 3` ran its 3 s). Steered while the
turn's *last* request is held, the turn makes one more request carrying it rather than dropping it.
Errors, all `-32600`: wrong id `expected active turn id \`X\` but found \`Y\``, missing id `Invalid
request: missing field \`expectedTurnId\``, unknown thread `thread not found: …`, no active turn
`no active turn to steer`. **`turn/start` during an active turn is a steer too**: it answers the
active turn's id with `status: inProgress` and the text joins the running turn; nothing is queued
and no `thread/queue/changed` is sent.

**P7 interrupt.** `turn/interrupt` answers `{}` immediately and `turn/completed` follows with
`status: "interrupted"`, `items: []`. **The running command is not killed**: `sleep` stayed alive,
and its `commandExecution` `item/completed` arrived ~20 s **after** the turn's `turn/completed`,
under the old `turnId`. The next turn's request carries `custom_tool_call_output: "aborted by user
after 1.4s"` and a developer `<turn_aborted>` note that background processes may still run.
Interrupting while a provider request is held also ends the turn at once. Interrupting a finished
turn: `-32600 "no active turn to interrupt"`; wrong id: `-32600 "expected active turn id …"`; but
**re-interrupting a just-interrupted turn got no response at all** (60 s) while its command was
still running. A new `turn/start` after an interrupt works.

**P8 resume.** An `ephemeral: false` thread has a rollout under
`$CODEX_HOME/sessions/…/rollout-…-<id>.jsonl` (`thread.path`). After SIGKILL and relaunch,
`thread/resume {threadId}` returns the thread with its turns, and the next turn's request carries
the earlier turn. Resuming a thread already loaded in the process succeeds. `thread/resume` accepts
`sandbox`/`approvalPolicy` overrides (`readOnly`, `on-request` echoed) and injects a fresh
environment context with the new permission profile. An ephemeral thread cannot be resumed after
relaunch: `-32600 "no rollout found for thread id …"`. **Cross-surface works both ways** on one
`CODEX_HOME`: a `codex exec` thread resumed by app-server carries its history, and
`codex exec resume <app-server thread id>` continues it under the same id.

**P9 lifecycle.** stdin EOF → exit 0 within 20 ms, idle or mid-turn (no `turn/completed` is sent
for the abandoned turn); SIGTERM mid-turn → exit 0. Every descendant — the bridge, the
`codex-code-mode-host`, the shell and its `sleep` — was gone 1 s later. With `features.plugins =
false` there was no plugin clone and no `git fetch`. stderr: tracing `ERROR` lines on failed tool
calls only.

**P10 opt-out.** `optOutNotificationMethods` suppresses exactly the listed methods for the
connection (`item/agentMessage/delta`, `item/commandExecution/outputDelta`,
`thread/tokenUsage/updated`, `item/started`, …); `item/completed`, `turn/*` and the rest still
arrive.
