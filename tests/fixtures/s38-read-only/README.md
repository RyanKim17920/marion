# s38: read-only node switches, measured against marion's canned provider

Measured 2026-09-27 on macOS 25.5.0. The model was marion's own canned provider
(`crates/marion-provider`), running as a small scratch binary (`_driver/s38-canned.main.rs.txt`) that
builds a `Script` from JSON: a `NodeScript` for the Anthropic and Chat Completions wires,
`gemini_edit` for Gemini, and `child_patch`/`child_exec_js` for Responses. No vendor endpoint, no
login and no real key were used; every key is a placeholder. Every launch carried its row's
no-self-update switch. Each harness ran in a fresh `git init` work dir with its home relocated the
way its row relocates it. `<SCRATCH>` and `<HOME>` stand for redacted paths.

Each case directory has these files:
- `<case>.argv.txt`: argv and env names only.
- `<case>.stdout.txt` and `<case>.stderr.txt`.
- `<case>.exit.txt`: the exit code, and whether `src/ro.txt` exists after the run.
- `<case>.provider.txt`: the request count and the tool names the harness declared to the model.
- `launcher.sh`: the wrapper that sets the env/config.

The negative cases **script the write anyway**, even when the tool is not declared. A real model
could not do that. It tests whether the harness enforces the switch, and not only whether it hides
the tool.

| harness | version | read-only switch | positive control | negative | how the refusal shows | status |
|---|---|---|---|---|---|---|
| claude | 2.1.283 | `--tools Read` (omit `Write`) | `--tools Read,Write --allowedTools Write`: file lands, exit 0 | file absent, exit 0 | `tool_result is_error:true`: `<tool_use_error>Error: No such tool available: Write. Write is disabled for this session, in subagents as well as here.</tool_use_error>`. `result.subtype: success`, `permission_denials: []` | measured |
| claude (alt) | 2.1.283 | `Write` declared, not in `--allowedTools`, no permission-prompt tool | (same) | file absent, exit 0 | `Claude requested permissions to write to … but you haven't granted it yet.`, plus `result.permission_denials[{tool_name: Write}]`. Under marion's `--permission-prompt-tool stdio` this would reach marion as `can_use_tool` instead | measured (not marion's argv) |
| codex | 0.155.1 | `sandbox_mode = "read-only"` (config.toml; live: `-c sandbox_mode="read-only"`) | `workspace-write`: `apply_patch` adds file, exit 0 | file absent, exit 0 | `custom_tool_call_output`: `Script error:\npatch rejected: writing is blocked by read-only sandbox; rejected by user approval settings`. Nothing in `--json` stdout marks it; stdout shows only the final agent_message | measured |
| codex exec_command | 0.155.1 | same | `workspace-write`: `printf s38 > src/ro.txt` lands, rc=0 | file absent | seatbelt: exec output `zsh:1: operation not permitted: src/ro.txt\nrc=1`, tool call itself `exit_code: 0` | measured |
| opencode | 1.18.32 | `OPENCODE_PERMISSION={"edit":"deny","bash":"deny"}`, or the same keys in the config `permission` block | default: `write` lands, exit 0 | file absent, exit 0; `write`, `edit`, `bash` gone from `tools[]` | `tool_use` frame with `tool: "invalid"`, output `Model tried to call unavailable tool 'write'. Available tools: glob, grep, invalid, read, skill, task, todowrite, webfetch.` | measured |
| opencode key check | 1.18.32 | `{"write":"deny"}` | n/a | **file lands**: `write` is not a permission key, and the write tool falls under `edit` | none | measured |
| opencode bash | 1.18.32 | `{"edit":"deny"}` alone | n/a | **bash `printf > src/ro.txt` lands**. `edit` alone is not read-only | none | measured |
| opencode bash | 1.18.32 | `{"edit":"deny","bash":"deny"}` | n/a | bash call refused, file absent | `bash` absent from `tools[]`, so the call becomes `invalid` | measured |
| opencode task | 1.18.32 | `{"edit":"deny","bash":"deny"}` | n/a | `task` subagent's `write` refused, file absent | subagent `tools[]` = glob, grep, read, skill, webfetch; its write becomes `invalid` | measured |
| gemini | 0.53.0 | default approval mode (no `--approval-mode auto_edit`) | `--approval-mode auto_edit`: `write_file` lands, exit 0 | file absent, exit 0 | stream `tool_result status:error, error.type: tool_not_registered`, `Tool "write_file" not found. Did you mean one of: …`. `result.status: success` | measured |
| gemini shell | 0.53.0 | default mode | n/a | `run_shell_command` not declared in **either** mode headless; file absent | same `tool_not_registered` | measured |
| gemini subagent | 0.53.0 | default mode | n/a | `invoke_agent generalist` subagent: its tools exclude `write_file`; file absent | subagent `tools[]` = list_directory, read_file, grep_search, glob, google_web_search, enter_plan_mode, complete_task | measured |
| qwen | 0.23.0 | `--core-tools` without `write_file` (kept `--yolo`) | `--yolo --core-tools read_file write_file` + absolute path: lands, exit 0 | file absent, exit 0 | `tool_result is_error:true`: `"write_file" is not listed in the active core tools allowlist (--core-tools or settings tools.core), so the tool is not available. …`, plus `result.permission_denials[{tool_name: write_file}]` | measured |
| qwen shell | 0.23.0 | same | n/a | `run_shell_command` refused the same way; file absent | same allowlist text | measured |
| qwen no-yolo | 0.23.0 | `write_file` in `--core-tools`, no `--yolo` | n/a | not even declared (`tools[]` = read_file); file absent | `Qwen Code requires permission to use "write_file", but that permission was declined. Matching deny rule: "edit".` | measured |
| pi | 0.80.2 | `--tools read` | `--tools read,write,edit`: lands, exit 0 | file absent, exit 0 | `tool_execution_end isError:true`, `Tool write not found` | measured |
| goose | 1.52.0 (row pinned 1.49.0) | no `--with-builtin developer` | `--with-builtin developer`, `GOOSE_MODE=auto`: `write` lands, exit 0 | file absent, exit 0 | `toolRequest.toolCall.status:error`, `-32600: Tool 'write' was not advertised for this model turn` | measured |
| goose chat | 1.52.0 | `GOOSE_MODE=chat` (developer loaded) | n/a | file absent, exit 0 | toolResult `success` carrying `Let the user know the tool call was skipped in goose chat mode…`. **This withholds every call, marion's `report` included**, so it cannot be the switch | measured |
| copilot | 1.0.83 | `--available-tools=view` (no create/edit/apply_patch) | `--available-tools=view,create,edit,apply_patch --allow-tool=write`: `create` lands, exit 0 | file absent, exit 0 | `tool.execution_complete success:false, error: {message: "Tool 'create' does not exist.", code: "failure"}` | measured |
| copilot deny | 1.0.83 | `--deny-tool=write` (over `--allow-tool=write`) | (same) | file absent, exit 0 | `error: {message: "Permission to run this tool was denied due to the following rules: \`write\`", code: "denied"}`. Deny wins over allow | measured |
| acp (opencode acp) | 1.18.32 | answer `session/request_permission` with `reject_once` | agent asks (config `permission {"edit":"ask"}`), client answers `allow_once`: lands | reject: file absent, `stopReason: end_turn` | `tool_call_update status:"failed"`, `The user rejected permission to use this specific tool call.`. Permission `toolCall.kind: "edit"`, options `allow_once, allow_always, reject_once` | measured |
| acp default | 1.18.32 | reject answer, opencode default permission | n/a | **file lands**: opencode's default is `allow`, so no `request_permission` is ever sent. Rejecting asks alone is not read-only | none | measured |
| acp mode | 1.18.32 | `session/set_config_option {configId:"mode", value:"plan"}` | n/a | file absent (tools still declared) | `tool_call_update failed`, `The user has specified a rule which prevents you from using this specific tool call. …` | measured |
| cline | not installed | none known. s27 item 12: tools cannot be narrowed. `--auto-approve false` declines **every** call, marion's `report` included | — | — | s27: `Tool "…" requires approval in a TTY session` | **unmeasured**: `cline` is not on PATH (no install anywhere found) |
| antigravity | 1.2.8 | default mode (no `--mode accept-edits`), or `--mode plan` | — | — | s32 (live login): headless auto-denies prompting tools, `status: SUCCESS`, `denied_actions`, `auto-denied` on stderr | **unmeasured**: no canned route (the row refuses canned). Measuring needs the operator's login and a paid model |

## Canned write recipes (tool name and args, per wire)

| harness | Script field | tool | args | existing test |
|---|---|---|---|---|
| claude | `anthropic_edit` (or `NodeScript`) | `Write` | `{file_path, content}` (a relative path works) | `crates/marion-supervisor/tests/cross_product.rs:421` |
| codex | `child_patch` (`apply_patch` via code-mode `exec`) | `apply_patch` | `*** Begin Patch\n*** Add File: <p>\n+<line>\n*** End Patch` | `cross_product.rs:429`; shell form `child_exec_js` in `timeout_kill.rs:129` |
| codex (shell) | `child_exec_js` | `tools.exec_command` | `const r = await tools.exec_command({cmd: "…"}); text(JSON.stringify(r));` | this fixture; `timeout_kill.rs:129` |
| opencode | `openai_edit` | `write` | `{filePath, content}` | `cross_product.rs:456`, `acp_child.rs:68` |
| opencode (shell) | `openai_edit`/`NodeScript` | `bash` | `{command, description}` | this fixture only |
| qwen | `openai_edit`/`NodeScript` | `write_file` | `{file_path: <ABSOLUTE>, content}` (a relative path is refused) | none canned (s25 probe only; `cross_product.rs` has no qwen child) |
| pi | `openai_edit` | `write` | `{path, content}` | `cross_product.rs:527` |
| goose | `openai_edit` | `write` | `{path, content}` | `cross_product.rs:490` |
| copilot | `openai_edit` | `create` | `{path, file_text}` | `cross_product.rs:476` |
| cline | `openai_edit` | `editor` | `{path, new_text}` (no `old_text`) | `cross_product.rs:503` |
| acp (opencode) | `openai_edit` | `write` | `{filePath, content}` | `acp_child.rs:68` |
| antigravity | none | `write_to_file` | — | none canned (no canned route) |

The canned file assertion is `comp.changed_paths` containing the file:
`cross_product.rs:1107-1111` and `acp_child.rs:143`.

**The gemini CLI captures were removed on 2026-09-30, when marion retired that harness** (Google
discontinued the CLI in favour of Antigravity). Its rows above stay as the measurement recorded them.
