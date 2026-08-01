# S6 — `codex exec --json` assumptions

Run 2026-08-01 against **codex-cli 0.146.0**, a **canned** OpenAI-Responses provider
(`spikes/s6/canned_provider.py`) and a real stdio MCP server (`spikes/s6/mcp_marion.py`).
**No model was called and no API key was used** — `model_providers.canned.base_url` points at
`127.0.0.1`, so the runs are free and repeatable.

All three questions are answered. Design doc §11 item 12 closes.

## 1. Does `exec` host MCP servers? — **YES**

`exec-mcp-report.stream.jsonl` shows a real `mcp_tool_call` item (`server: "marion"`,
`tool: "report"`) whose `result` is the text this repo's own MCP server returned.
`mcp-server-frames.jsonl` is that server's side: `initialize` (client
`codex-mcp-client/0.146.0`, protocol `2025-06-18`), `tools/list`, then `tools/call`.

**marion's `report` return path exists. M1 takes the primary branch, not the fallback.**

Two operational details worth keeping:

- **codex spawns the server more than once per run** — the frame log holds two full
  `initialize` + `tools/list` sequences for one `codex exec`. A bridge must tolerate that.
- **`tools/call` carries `_meta.x-codex-turn-metadata`** with `session_id`, `thread_id`,
  `turn_id`, `sandbox` (`"seatbelt"`), and per-workspace `latest_git_commit_hash` +
  `has_changes`. marion gets the child's own view of the repo state for free, on the same call
  that delivers the report.

## 2. Does `exec --json` emit file locations? — **YES**, as `file_change` items

`exec-codemode-apply-patch.stream.jsonl` carries

```json
{"type":"file_change","changes":[{"path":"<HOME>/…/wt/a.rs","kind":"update"}],"status":"completed"}
```

emitted at both `item.started` and `item.completed`. Absolute paths, with a `kind`
(`update`). This is corroborating evidence only — §6.7 keeps git as the authority for
`changed_paths`.

## 3. Does `--output-schema` / `--output-last-message` deliver a document? — **YES**

`output-last-message.txt` holds the exact JSON the canned provider emitted as the final
message. `provider-requests.redacted.jsonl` shows the schema forwarded as
`text.format = {type: json_schema, strict: true, name: "codex_output_schema", schema: …}`.

**`strict: true` is added by codex**, so every key in `properties` must also appear in
`required` — the schema used here expresses optionality as `anyOf [array, null]` and is
accepted. The natural spelling (`result_commits` optional) is rejected by the endpoint, not by
the mechanism; see §12.

## The finding that changes M1: **code mode**

Codex 0.146.0 does **not** declare individual tools in a top-level `tools` field. There is no
such field. Tools arrive inside `input` as an `additional_tools` developer message, and the
model's actual surface is a **`custom` tool named `exec` that runs JavaScript in a V8 isolate**:

> All nested tools are available on the global `tools` object, for example
> `await tools.exec_command(...)`. Tool names are exposed as normalized JavaScript identifiers,
> for example `await tools.mcp__ologs__get_profile(...)`.

So a real model reaches marion by writing `await tools.mcp__marion__report({...})`, and
`client_metadata.x-codex-turn-metadata.code_mode_tool_names` carries the mapping:

```json
"mcp__marion__report": {"name": "report", "namespace": "mcp__marion"}
```

**Both spellings are real, at different layers** — the flat `mcp__marion__report` is the
JavaScript identifier; `{name, namespace}` is what codex dispatches internally.

**For marion's canned provider this is good news:** a plain
`{"type":"function_call","name":"report","namespace":"mcp__marion"}` item **is executed** —
`exec-mcp-report.stream.jsonl` proves it end to end. marion's scripts do not have to synthesise
JavaScript; the direct form works and is far easier to author.

### `apply_patch` under code mode

`tools.apply_patch` takes a **string**, not an object. Passing `{input: …}` fails with

> `Script error: tool `apply_patch` expects a string input`

The working call is `await tools.apply_patch("*** Begin Patch\n*** Update File: …\n@@\n-…\n+…\n*** End Patch")`,
and it really edits the file (verified by `git diff` in the scratch worktree). The `exec`
contract also documents `exit()`, `text()`, `image()`, `store()`/`load()`, and an optional
`// @exec: {"yield_time_ms":…, "max_output_tokens":…}` first-line pragma.

## Reproducing

```sh
cd spikes/s6
S6_SCRIPT=toolcall python3 canned_provider.py &        # or: text | patch
CODEX_HOME=$PWD/codex-home S6_DUMMY_KEY=dummy \
  codex exec --json --skip-git-repo-check -C $PWD/wt "call the report tool" </dev/null
```

## Redaction

Home paths → `<HOME>`, all UUIDs → `<UUID>`. The provider request log is **reduced**, not just
scrubbed: only `model`, `tool_choice`, `parallel_tool_calls`, `text`, the `additional_tools`
block and `code_mode_tool_names` are kept. The full body carries the operator's skill catalogue,
installation id and environment context, none of which the evidence needs.
