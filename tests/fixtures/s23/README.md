# S23 — `opencode acp` against a canned local provider, at $0.00

Run:

```
python3 spikes/s23/run.py                      # -> tests/fixtures/s23/opencode-acp-canned-*
S23_DEAD_PROXY=1 S23_PREFIX=opencode-acp-canned-deadproxy python3 spikes/s23/run.py
```

Measured 2026-08-08, macOS darwin 25.5.0, `opencode` 1.17.3 (`/opt/homebrew/bin/opencode`).

## Why this was measured

S21 drove `opencode acp` to a `marion_report` call, but on the operator's **live login and a real
model**. S13 drove `opencode run` against a **canned** OpenAI-compatible endpoint at $0.00. A doc
comment in `crates/marion-harness/src/acp.rs` asserted the composition of the two as a measured
fact. It was not one: there was no `tests/fixtures/s23`, and the composition is not free —
`session/new` is a different surface from argv, and marion's bridge rides the *protocol* there
rather than the config document, so the config document and the MCP declaration arrive by two
different channels for the first time.

The question: can `opencode acp`, under an isolated `$XDG_CONFIG_HOME` carrying exactly the
document `opencode::config_json` emits, complete `initialize -> session/new -> session/prompt` with
the model calling the MCP tool declared over ACP — with no network and no credential.

## What was found

**Yes. End to end, on the first configuration tried, in 1.9 s, with three POSTs and all three to
127.0.0.1.**

| | measured |
|---|---|
| config channel | `$XDG_CONFIG_HOME/opencode/opencode.json` — the ACP surface honours it exactly as `run` does |
| MCP channel | ACP `session/new` `mcpServers` — **no `mcp` block in the document** |
| provider traffic | 3 × `POST http://127.0.0.1:8123/v1/chat/completions`, `stream: true`, `model: "canned-1"` |
| credential presented | the minted `sk-marion-s23-canned`, as `Authorization` (logged redacted, 27 chars) |
| **the name the model typed** | **`marion_report`** — and the agent *declared* it to the provider under that name too |
| the name on the MCP wire | **`report`**, unprefixed — S13's two-layer split, unchanged on this surface |
| turn result | `"stopReason": "end_turn"` |
| cost | **$0.00** — no vendor endpoint, no real key, no `auth.json` anywhere in the sandbox |

Both runs (plain and dead-proxy) produced the identical sequence, identical tool call and identical
`stopReason`.

### The document that worked

A field-for-field transcription of `opencode::config_json(&ConfigSpec{ model: canned/canned-1,
base_url: "http://127.0.0.1:8123/v1", api_key: Some("sk-marion-s23-canned") }, None)` — hand-written
because the spike may not add code to `crates/`. It is not a guess at that function's output: the
crate's own unit tests (`the_provider_block_names_the_bundled_openai_compatible_adapter`,
`the_provider_carries_the_timeouts_that_are_the_first_line_of_defence`,
`a_config_without_an_mcp_server_still_configures_the_provider`) assert every key here by name, on a
`ConfigSpec` that already uses `canned/canned-1`, and only the base URL and key differ. Keys are
sorted because serde_json without `preserve_order` emits a `BTreeMap`. Also on disk as
`opencode.json` here:

```json
{"model":"canned/canned-1","provider":{"canned":{"models":{"canned-1":{"name":"canned-1","tool_call":true}},"name":"canned","npm":"@ai-sdk/openai-compatible","options":{"apiKey":"sk-marion-s23-canned","baseURL":"http://127.0.0.1:8123/v1","headerTimeout":30000,"timeout":120000}}},"small_model":"canned/canned-1"}
```

with `isolation_env(sandbox, Auth::Canned)` verbatim — the five relocations plus the eight
`OPENCODE_*` / `OPENCODE_DB` entries — and nothing else inherited but `PATH`, `TMPDIR`, `LANG`,
`TERM`.

### The verbatim tool call

Provider side — the canned model emits the name it read out of the agent's own `tools[]` array,
never a guess (`spikes/s23/canned_provider.py` logs `turn-no-marion-tool` and calls nothing if the
array lacks it, so this spike cannot pass with the bridge disconnected):

```json
{"type":"function","function":{"name":"marion_report","description":"Report the outcome of this task back to marion.","parameters":{"type":"object","properties":{"narrative":{"type":"string"},"result_commits":{"type":"array","items":{"type":"string"}}},"required":["narrative"],"additionalProperties":false}}}
```

ACP side — S21's two-frame shape, reproduced exactly:

```
{"sessionUpdate":"tool_call",       "toolCallId":"call_s23_marion_1","title":"marion_report","kind":"other","status":"pending","locations":[],"rawInput":{}}
{"sessionUpdate":"tool_call_update","toolCallId":"call_s23_marion_1","status":"in_progress","kind":"other","title":"marion_report","locations":[],"rawInput":{"narrative":"hello from acp under a canned provider"}}
{"sessionUpdate":"tool_call_update","toolCallId":"call_s23_marion_1","status":"completed","kind":"other","title":"","content":[{"type":"content","content":{"type":"text","text":"recorded"}}],"rawInput":{"narrative":"hello from acp under a canned provider"},"rawOutput":{"output":"recorded","metadata":{"truncated":false}}}
```

MCP side:

```
{"dir":"in","frame":{"method":"tools/call","params":{"name":"report","arguments":{"narrative":"hello from acp under a canned provider"}},"jsonrpc":"2.0","id":2}}
```

And the response:

```json
{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn","usage":{"inputTokens":0,"outputTokens":0,"totalTokens":0},"_meta":{}}}
```

## Four things that differ from `opencode run` under the same document

1. **`toolCallId` is the provider's id, passed straight through.** `call_s23_marion_1` is a string
   the canned provider minted; opencode did not rewrite it. S21's live run showed an
   opencode-generated `call_00_3xFF…` only because a real provider generated it. This makes an ACP
   fixture **deterministic**, which is what lets S23 be an assertion in CI rather than a snapshot
   that churns.
2. **`usage` comes back all zeros.** S21 recorded `{inputTokens: 113, outputTokens: 4, totalTokens:
   14581}`. Here the canned provider sends no `usage` in its SSE and opencode reports zeros without
   complaint. So `advertised(Harness::Acp, Capability::Usage)` rests on the *field being present*,
   not on it being non-zero, and a test that asserted non-zero would only be asserting that a real
   vendor was billed.
3. **There is no `--title` on `acp`.** S13 made `--title` *required* on the `run` invocation because
   without it opencode issues a third `You are a title generator` POST against `small_model`. On ACP
   there is no argv to put it in: the side request fires (captured, `kind: "side-stub"`, arriving
   *between* the two real turns). Pinning `small_model` to the same `provider/model` is therefore
   **load-bearing on this surface, not belt-and-braces** — it is the only thing keeping that request
   on marion's endpoint.
4. **`session/new` hands the client a model selector marion never configured.** The response's
   `configOptions` lists six models: `canned/canned-1` (the `currentValue`) plus five
   `opencode/*` OpenCode Zen entries, offered from a catalogue bundled in the binary — with
   `OPENCODE_DISABLE_MODELS_FETCH=1` set and the sandbox's `cache` still 0 B, so no fetch happened.
   The `run` surface exposes nothing analogous. A `set_config_option` from the client could move an
   ACP node onto a provider marion did not configure; marion's own client simply never sends one,
   but the ability is the agent's, not marion's, and that is a fact about the surface rather than
   about the document.

Also observed, in both runs: after the `tools/call` result is returned, the agent sends the MCP
server a `notifications/cancelled` for that same `requestId` — `{"reason": "AbortError: The
operation was aborted."}`. It arrives at teardown, after the answer was already used (the narrative
reached the model and the turn ended `end_turn`), so it is cosmetic here. It is recorded because a
bridge that treated a post-hoc `cancelled` as "the call did not happen" would drop a report it had
already delivered.

## Files

* `opencode.json` — the config document written to `$XDG_CONFIG_HOME/opencode/`.
* `opencode-acp-canned-session.jsonl` — the agent's own stdout, verbatim, one frame per line.
* `opencode-acp-canned-mcp.jsonl` — the MCP server's transcript, both directions.
* `opencode-acp-canned-prompt-response.json` — the `session/prompt` response.
* `opencode-acp-canned-provider-requests.jsonl` — every HTTP request the agent made to the canned
  provider: path, method, dispatch kind, the tool names it declared, the full body, and the headers
  with `Authorization` replaced by a length marker.
* `opencode-acp-canned-stderr.txt` — the agent's stderr (empty).
* `opencode-acp-canned-deadproxy-*` — the same five, from the dead-proxy run.

## What was not measured

* **Zero packets leaving the host is not proven.** What is proven is that every model request went
  to `127.0.0.1:8123` (the provider log is the complete set of requests it received, and the turn
  could not have completed without them), that the sandbox contains no `auth.json` and no vendor
  credential, and that the run completes **identically** with `HTTP_PROXY`/`HTTPS_PROXY` pointed at
  a closed port and `NO_PROXY=127.0.0.1` — so nothing off-loopback was load-bearing. A swallowed,
  non-load-bearing attempt would look the same, and a packet filter is what would rule it out.
* **`gemini --acp` was not attempted.** S20's `session/new` refusal fires before any provider
  configuration matters; a canned endpoint cannot reach past it.
* **No permission, interrupt or steer path was driven.** As in S21, the ACP capabilities that rest
  on observation here are `token_deltas` (`agent_message_chunk`, one chunk in this run) and `usage`
  (present, zeroed — see above). The rest stay `false`.
