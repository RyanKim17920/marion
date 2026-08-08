# S21 — drive a real ACP agent to a marion tool call

Run:

```
python3 spikes/s21/acp_child_probe.py opencode -- opencode acp
python3 spikes/s21/acp_child_probe.py gemini   -- gemini --acp   # blocked at session/new, see S20
```

Measured 2026-08-08, macOS darwin 25.5.0, `opencode` 1.17.3.

## Why this was measured

S20 answered *"can an ACP agent open a session here"* and stopped there. Two questions it left open
are the ones an ACP `HarnessAdapter` cannot be written without:

1. **Does `session/new`'s `mcpServers` actually reach the agent?** `89b822d` declined to write the
   adapter partly on the grounds that a fifth `McpRoute` variant would sit in the supervisor's
   declaration check *"with nothing behind it to check"*. That is a factual claim, and it had never
   been tested.
2. **What name does the model type for a marion verb?** s14's finding is that claude, gemini and
   opencode all **silently ignore** an unknown tool name — so a guessed spelling produces a run
   that looks healthy, exits 0, and called nothing. The repo's rule is that a verb ships only with
   a measured mapping, and no ACP mapping had ever been measured.

The probe therefore declares a **real MCP stdio server** (`spikes/s21/mcp_echo_server.py`, offering
exactly one tool, `report`) in `session/new`, then sends a `session/prompt` telling the model to
call it, and records both wires verbatim.

## What was found

**`opencode acp` goes all the way through, and both questions are answered.**

| | measured |
|---|---|
| `session/new` with `mcpServers` | accepted; `sessionId` `ses_01ea7e713ffe…` |
| the agent → marion's MCP server | `initialize` → `notifications/initialized` → `tools/list` → `tools/call` |
| **the name the model typed** | **`marion_report`** — `<server>_<tool>`, in the `session/update` `tool_call` frame's `title` |
| the name on the MCP wire | **`report`**, unprefixed — the two-layer split S13 measured on `opencode run`, one surface up |
| turn result | `stopReason: "end_turn"`, `usage {inputTokens: 113, outputTokens: 4, totalTokens: 14581}` |

So there **is** something behind `McpRoute::Session`: the declaration marion compiles starts a
process, and that process is asked for its tools and then called.

### The frame shape, and the trap in it

A marion call spans **two** `session/update` frames and neither is sufficient alone:

```
{"sessionUpdate":"tool_call",       "toolCallId":"call_00_3xFF…","title":"marion_report","status":"pending","rawInput":{}}
{"sessionUpdate":"tool_call_update","toolCallId":"call_00_3xFF…","status":"in_progress","title":"marion_report","rawInput":{"narrative":"hello from acp"}}
{"sessionUpdate":"tool_call_update","toolCallId":"call_00_3xFF…","status":"completed","title":"","rawInput":{"narrative":"hello from acp"},"content":[…]}
```

The **terminal** frame carries `"title": ""`. A reader that took the last frame — which is what
opencode's own `run` reader does, since that stream emits only terminal states — would find an
outcome attached to no verb. So `acp::marion_calls` pairs by `toolCallId`, and
`neither_half_of_a_call_can_be_read_without_the_other` is that pairing as an assertion.

`rawInput` is revised in place: `{}` on the opening frame, the real arguments later. So the last
non-empty one wins, and it is read only from frames whose `toolCallId` was **opened as `report`** —
otherwise another tool's arguments would be filed as marion's narrative.

### Two capabilities, measured rather than assumed

* **`token_deltas`** — `agent_message_chunk` and `agent_thought_chunk` arrive one token at a time
  (`"The"`, `" user"`, `" wants"`, …).
* **`usage`** — a `session/update` `usage_update` (`used`, `size`, `cost`) plus a `usage` object on
  the `session/prompt` response.

Those are the two `advertised(Harness::Acp, _)` entries that rest on an observation rather than on
the protocol's shape. `steer`, `interrupt`, `permissions`, `elicitation` and `set_model` are all
defined by ACP and **none of them was driven here**, so all five stay `false`.

## Files

* `opencode-acp-session.jsonl` — the agent's own stdout, verbatim, one frame per line.
* `opencode-acp-mcp.jsonl` — the MCP server's transcript, both directions.
* `opencode-acp-prompt-response.json` — the `session/prompt` response.

## What was not measured

`gemini --acp` never reaches a turn: S20's `session/new` refusal (`-32000`, Gemini Code Assist
ineligibility) fires before any tool exists to name. So **gemini's ACP tool spelling is unknown**,
and `acp::GEMINI.tools` is `None`. `AcpAdapter` refuses to declare marion's bridge to it by name
rather than reusing opencode's spelling, which is exactly the guess s14 says would be swallowed.
