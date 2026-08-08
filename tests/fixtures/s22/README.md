# S22 — the ACP Registry's shims, and the third and fourth spelling of one tool

Run:

```
python3 spikes/s21/acp_child_probe.py claude-shim -- npx -y @agentclientprotocol/claude-agent-acp@0.66.0
npm i --prefix /tmp/codexacp @agentclientprotocol/codex-acp@1.1.14   # see "the npx trap" below
python3 spikes/s21/acp_child_probe.py codex-shim  -- /tmp/codexacp/node_modules/.bin/codex-acp
```

Measured 2026-08-08, macOS darwin 25.5.0, node 26.7.0, against the operator's own logged-in
`claude` 2.1.220 and `codex` 0.147.0. No new credential was supplied for either.

## Why this was measured

S20 counted **one** ACP agent beyond the day-one set that can open a session here (`opencode acp`),
and recorded `gemini --acp` as blocked vendor-side. §9's M5 asks for two. The open question was
whether the ACP Registry's *shims* — published adapters that wrap a non-ACP vendor CLI in an ACP
stdio server — count as agents that run here, or whether they are a paper listing.

They are not a paper listing. **Both shims complete `initialize`, open a session, and run a real
turn that calls marion's MCP tool**, against credentials that were already on this machine.

## What was found

| | `claude-agent-acp` 0.66.0 | `codex-acp` 1.1.14 |
|---|---|---|
| `agentInfo.name` | `@agentclientprotocol/claude-agent-acp` | `@agentclientprotocol/codex-acp` |
| `agentInfo.version` | `0.66.0` | `1.1.14` |
| `protocolVersion` | 1 | 1 |
| `initialize` | ok | ok |
| **`session/new`** | **ok** — `sessionId` returned | **ok** — `sessionId` returned |
| **`session/prompt`** | **`stopReason: "end_turn"`** | **`stopReason: "end_turn"`** |
| `loadSession` | true | true |
| `sessionCapabilities` | `{additionalDirectories, close, delete, **fork**, list, resume}` | `{additionalDirectories, close, delete, list, resume}` — **no `fork`** |
| `promptCapabilities` | `{image, embeddedContext}` | `{image, embeddedContext}` |
| `mcpCapabilities` | `{http: true, sse: true}` | `{acp: false, http: true, **sse: false**}` |
| `authMethods` | `[]` | `api-key`, `chat-gpt` |
| **the name the model typed** | **`mcp__marion__report`** | **`mcp.marion.report`** |
| the name on the MCP wire | `report`, unprefixed | `report`, unprefixed |
| termination on stdin EOF | **exit 0** in 0.04 s | **exit 0** in 2.05 s |

Verbatim: `*-initialize.json`, `*-session.jsonl`, `*-mcp.jsonl`, `*-prompt-response.json`.

The termination row is a separate measurement from the transcripts above (the probe kills its child
rather than closing stdin, so those runs cannot answer it): `initialize`, then close stdin, then
`wait`. Both shims exit **0**, so neither needs a kill to be reaped — which is what the ACP
Registry's `src/index.ts` claims and what `doctor`'s `AcpChild::Drop` would otherwise have to cover.

### The finding: three ACP agents, three spellings, and none of them guessable

This is the third independent measurement of *"what name does the model type for a marion verb"*
on the ACP surface, and it is the third different answer:

| agent | spelling |
|---|---|
| `opencode acp` 1.17.3 (S21) | `marion_report` |
| `claude-agent-acp` 0.66.0 | `mcp__marion__report` |
| `codex-acp` 1.1.14 | `mcp.marion.report` |

`acp.rs`'s `ToolSpelling` doc says a second agent adds a variant *"only when somebody has watched it
call a tool"*, and `AcpAdapter::bridgeable_agent` refuses an unmeasured agent rather than reusing
opencode's form. **Both were right, and reusing the form would have been wrong twice over.** s14's
finding is that an unknown tool name is silently ignored, so an ACP adapter that had generalised
`<server>_<tool>` would have compiled `marion_report` into a `claude-agent-acp` prompt, watched the
turn end `end_turn`, and recorded a healthy run that called nothing.

### `codex-acp` does not flatten the name at all

The other two put a flat string in `title` and the arguments in `rawInput`. `codex-acp` reports the
call as an **`execute`** kind whose `rawInput` is *structured*:

```json
{"sessionUpdate":"tool_call","toolCallId":"exec-…","kind":"execute","title":"mcp.marion.report",
 "rawInput":{"server":"marion","tool":"report","arguments":{"narrative":"hello from acp"}},
 "_meta":{"is_mcp_tool_call":true}}
```

So the narrative is at `rawInput.arguments.narrative`, one level deeper than
`acp::parse_stream` reads it. A reader written against S21's shape finds the call and reads no
arguments from it.

### The terminal frame, again — and a new trap next to it

S21 found `opencode`'s terminal `tool_call_update` carries `"title": ""`. Neither shim reproduces
that exactly, and both reproduce the underlying hazard: **the terminal frame carries no `title` key
at all.**

```
claude-agent-acp  {"toolCallId":"toolu_…0001","sessionUpdate":"tool_call_update","status":"completed","rawOutput":[…]}
codex-acp         {"toolCallId":"exec-…0002","sessionUpdate":"tool_call_update","status":"completed","rawOutput":{…}}
```

Pairing by `toolCallId` — which `acp::marion_calls` already does — is therefore load-bearing on all
three agents, and is now measured on three rather than argued from one.

The new trap is in `codex-acp`'s **first** frame, before the session is even in use:

```json
{"sessionUpdate":"tool_call","toolCallId":"mcp_startup.marion","kind":"other",
 "title":"mcp__marion__startup","status":"failed",
 "content":[{"type":"content","content":{"type":"text","text":
   "[codex-acp forwarded startup error] MCP server `marion` startup was cancelled."}}]}
```

That is a *startup diagnostic*, not a call the model made — and it is titled with the **claude
shim's** `mcp__<server>__<tool>` spelling, on the codex shim, for a verb (`startup`) marion does not
have. A reader that matched marion's calls by the prefix `mcp__marion__` would pick this frame up as
a failed marion verb and report the turn as a refusal. It matched marion's server name and nothing
else about it was real. (The startup then succeeded: the MCP transcript shows the full
`initialize` → `tools/list` → `tools/call` sequence in the same session.)

### Both shims reached the vendor CLI, and the vendor version is only visible on the MCP wire

`clientInfo` on the MCP transcript is the wrapped CLI naming *itself*:

```
claude-agent-acp → {"name":"claude-code","title":"Claude Code","version":"2.1.220"}
codex-acp        → {"name":"codex-mcp-client","title":"Codex","version":"0.147.0"}
```

This is the evidence that the shim really is a wrapper over the local CLI, and it is **the only
place the vendor version appears**. `initialize`'s `agentInfo.version` is the *shim's* version and
says nothing about what it wraps. So at handshake time — the moment §3.3 keys a capability row —
marion can observe the shim leg of the chain and cannot observe the vendor leg. A row keyed on
`agentInfo.version` alone silently merges `shim 0.66.0 + claude 2.1.220` with
`shim 0.66.0 + claude <anything else>`.

### The npx trap

The first `codex-acp` probe returned **zero frames** and looked like a dead agent. It was not:
`npx -y` was still downloading `@openai/codex` (a large platform binary) when the probe's 30 s
`initialize` budget expired. Installed first and run from `node_modules/.bin`, the same version
handshakes in under a second. A cold `npx` is not a measurement of the agent.

## What this does **not** show

* **Neither shim was run as a marion child.** These are `spikes/s21/acp_child_probe.py` runs. No
  marion binary can spawn an ACP node today: `AgentType` has no field carrying an ACP agent id,
  `run.rs` and `root.rs` both construct `Extras::default()`, and no built-in agent type names
  `Harness::Acp`. ACP is reachable only from `marion-supervisor doctor`.
* **`Auth::Canned` is still refused** by `AcpAdapter::compile`, so even with plumbing these agents
  run only against the operator's own login.
* `steer`, `interrupt`, `permissions`, `elicitation` and `set_model` were **not driven**, exactly as
  in S21 — except that both shims *did* send `session/request_permission`, which the probe answered
  permissively. That is the client half being exercised, not marion's.

## Redaction

Per `tests/fixtures/REVIEW.md` §3. `sessionId`/`thread_id`/`turn_id` → stable fakes;
`toolCallId` and `messageId` → stable fakes that **preserve the vendor's id prefix** (`toolu_`,
`exec-`, `mcp_startup.`) because the prefix is how a reader tells kinds apart, and remain distinct
because the pairing is the evidence; `turn_started_at_unix_ms` and `updatedAt` → fixed values;
`/tmp/...` → `<SCRATCH>`; `/Users/...` → `<HOME>`.

The `available_commands_update` frame is **stubbed to one illustrative entry**. On these two agents
it was 38 KB and 84 KB of the operator's own skill, plugin and command catalogue — §7.1's *"the
operator's environment"* class, and it carried an account display name. The frame is kept because
its presence is a protocol fact; its contents were never the measurement.

Token counts are **kept**, as in S21: they are the evidence behind the `usage` capability.
