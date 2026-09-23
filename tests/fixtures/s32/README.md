# s32 — agy (Google Antigravity CLI) 1.2.8, headless `-p --output-format stream-json`

Measured 2026-09-22 on the operator's own keychain login (no login flow, no HOME relocation),
model `gemini-3.6-flash-low`, the MCP server a stdio probe named `marion`. Paths are redacted to
`<SCRATCH>`, `<TMP>` and `<HOME>`.

- **Declaration.** `--add-dir <root>` with `<root>/.agents/mcp_config.json` =
  `{"mcpServers":{"marion":{command,args,env}}}` loads the server. There is no MCP-config flag and
  no ACP. agy caches each server's tool schemas under `~/.gemini/antigravity-cli/mcp/<server>/`.
- **Workspaces sort.** With two `--add-dir` roots the model treats the lexicographically first as
  "the current directory" (5 of 5 runs: `aa-root`/`root-base` won over `ws-*`, `ws-*` over
  `zz-root`), and with none it does not know its cwd at all (it runs `pwd`, which headless denies).
  So a launch passes `--add-dir <cwd>` and the root, and states the working directory in the
  prompt; with that preamble the write lands in the cwd even when the root sorts first
  (`write-then-report.stream.jsonl`).
- **Approval.** Headless mode auto-denies any tool whose permission it would prompt for, exits 0
  with `status: SUCCESS`, lists `denied_actions` and says `auto-denied` on stderr. Not approving
  marion's tools: `--mode accept-edits` (it does approve `write_to_file`), `--sandbox`,
  `ANTIGRAVITY_PERM_GRANTS=mcp(marion/*)`, a `PreToolUse` hook in the root's `.agents/hooks.json`
  answering `allow` (the call then ends `DONE` with no output — `report-done-without-output`) or
  `ask` with `permissionOverrides`. No settings-path flag or variable exists. What approves it is
  the operator's own `~/.gemini/antigravity-cli/settings.json` carrying
  `"permissions": {"allow": ["mcp(marion/*)"]}` (`report-answered.stream.jsonl`); without it the
  call ends `ERROR` with `permission check failed for mcp "marion/report"`
  (`report-denied-error.stream.jsonl`).
- **Stream.** `init{conversation_id, init{model,cwd,tools,permission_mode}}`, then
  `step_update{step_index, state ACTIVE|DONE|ERROR, step_type, tool_name, tool_info{name,
  parameters, output|error}, text_delta, usage}`, then `result{conversation_id, status, response,
  num_turns, usage, denied_actions?}`. A marion call is `tool_name: call_mcp_tool` with
  `parameters{ServerName, ToolName, Arguments}`. `usage.input_tokens` excludes cache reads.
- **Resume.** `--conversation <id>` continues the conversation under the same id and remembers it
  (`resume-known`); an unknown id starts a **new** conversation with a fresh id, exit 0, and says
  so only as a stderr warning (`resume-unknown`, `resume-unknown.stderr.txt`).
- **Updates.** Only `AGY_CLI_DISABLE_AUTO_UPDATE=true` disables the self-update.
- **Push.** A `notifications/message` pushed after a call reached the pipe and was surfaced nowhere.
- **clientInfo.name** is `antigravity-client`.
