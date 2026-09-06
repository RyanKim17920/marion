# S25 — the installed ACP agents nobody had driven, and the fourth spelling of one tool

Measured 2026-09-05, macOS darwin 25.5.0, against the operator's own logins, with a throwaway
stdio MCP server named `marion` exposing one tool, `report`. Every file is the agent's own stdout,
verbatim, or the MCP server's own inbound log, verbatim. Nothing here is hand-made.

## Why this was measured

Four ACP agents had been watched calling `report` (S21, S22) and they spelled it three ways. The
question this answers is whether a **fifth** agent, one marion has no row for, can be read at all —
and what the installed agents nobody had driven do when handed marion's bridge the protocol's way,
in `session/new`'s `mcpServers`.

## What was found

| agent | `initialize` | `session/new` | the name the model typed | bridge started from `session/new`? |
|---|---|---|---|---|
| `copilot --acp` 1.0.83 | ok, `loadSession: true` | **ok** | **`marion-report`** (`<server>-<tool>`) | **no** — see below |
| `qwen --acp` 0.23.0 | ok, `loadSession: true` | **refused** `-32000` *"Authentication required: Use Qwen Code CLI to authenticate first"* | — | — |
| `goose acp` 1.49.0 | ok, `loadSession: true` | **refused** `-32603` *"Failed to resolve provider: Configuration value not found: GOOSE_PROVIDER"* | — | — |
| `gemini --acp` 0.53.0 | ok | **refused** `-32000` (Gemini Code Assist ineligibility, as S20) | — | — |

Three of the four refusals are **vendor-side and named in the agent's own words**, exactly the
shape ACP gives them: an `authMethods` list in `initialize` and a JSON-RPC error on `session/new`.
None is a fact about marion's client, and no adapter could route around one — which is what the
generic path's refusal-by-name exists to surface rather than hide.

### copilot: the session-declared bridge is ignored, the argv-declared one is taken

`copilot-acp-session-new-mcp-ignored.jsonl` — `session/new` declared the `marion` server in
`mcpServers` (stdio: `name`, `command`, `args`, `env` as `[{name, value}]`, the shape S21 measured
`opencode acp` starting). The session opened, the turn ran to `end_turn`, the model loaded its
`mcp2cli` skill and then said, in as many words, *"I can't call the `marion` MCP server because its
`report` tool is not available in this session."* The MCP server's log is **empty**: it was never
started. Repeated with `"type": "stdio"` on the declaration and `--allow-all-tools` on argv: same
result, zero frames.

`copilot-acp-session.jsonl` + `copilot-acp-mcp.jsonl` — the same binary, the same `session/new`
(still declaring `marion`, still ignored), plus copilot's **own** channel on argv:

```
copilot --acp --allow-all-tools --additional-mcp-config '{"mcpServers":{"marion":{"type":"stdio","command":"…","args":["…"],"tools":["*"]}}}'
```

The argv-declared server received `server/discover`, `initialize`, `notifications/initialized`,
`tools/list` and `tools/call {"name":"report","arguments":{"narrative":"hello from acp"}}`; the
ACP transcript shows one `tool_call` titled **`marion-report`** with a flat `rawInput`
`{"narrative":"hello from acp"}`, then `end_turn`.

So copilot's ACP server is a real ACP agent with a real quirk: on 1.0.83 the protocol-standard
declaration channel does nothing and the bridge reaches it only through the same
`--additional-mcp-config` document marion's copilot adapter already compiles for `copilot -p`.
That is a refinement-row fact (`acp::COPILOT`), and a generic `acp:copilot --acp` launch would open
a session, take the turn, and report nothing.

### The finding: four spellings, one pair

| agent | spelling |
|---|---|
| `opencode acp` 1.17.3 (S21) | `marion_report` |
| `claude-agent-acp` 0.66.0 (S22) | `mcp__marion__report` |
| `codex-acp` 1.1.14 (S22) | `mcp.marion.report` |
| `copilot --acp` 1.0.83 (S25) | `marion-report` |

Every one of them is marion's server alias and the verb, joined by some separator, sometimes under
an `mcp` prefix. That pair — not any one spelling — is what `acp::Reading::Generic` recognises, and
`the_generic_reading_finds_every_measured_agents_report` asserts it finds all four captures above.
