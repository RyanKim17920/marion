# S35: can a marion-owned Stop hook gate a native claude session?

Measured 2026-09-27 on macOS darwin 25.5.0 with **claude 2.1.283** (the admitted pin), on the
operator's existing claude.ai (Max) login, running interactive in a pty through
`marion-demo/tooling/final/driver` against a scratch git project. Model: haiku. Reported cost
across the four sessions was $0.171 (a $0.033, e $0.025, f $0.020, g $0.094). That figure is
notional because the login is a subscription. The operator's settings were read and never edited.
The trust dialog defaults to "No, exit", so the prelude moves to "Yes" and presses Enter only after
the screen shows the selection (`prelude.steps`).

Every session runs the shape marion's native claude uses: `--dangerously-load-development-channels
server:s35probe` last, with `channel-server.py` (a no-tool MCP server that declares
`experimental["claude/channel"]`) behind `--strict-mcp-config`. The debug log confirms
`Channel notifications registered`. The gate is `gate-hook.sh`, a Stop hook that logs its stdin
and, the first time, prints `{"decision":"block","reason":"GATE-<tag>: ... reply with exactly
PINEAPPLE-<tag>"}`. `run.sh` reruns everything.

| case | overlay | result | evidence |
|---|---|---|---|
| a | `--settings '{"hooks":{"Stop":[gate]}}'` | **(a) fires. (b) the block continues the turn and the reason reaches the model:** the model answers `OK`, the hook blocks, and the model then answers `PINEAPPLE-gate`. The second Stop call carries `"stop_hook_active":true`, and the hook allows it. **(c) coexists:** "Ran 5 stop hooks", meaning the gate plus the operator's user-settings `agent-deck` Stop hook (async) and the codex, ralph-wiggum and warp plugin Stop hooks. The gate's block is honoured alongside them. **(d) other keys survive:** the operator's statusLine renders, and their user `UserPromptSubmit` and `SessionStart` hooks still inject context, so a hooks-only overlay merges with user settings and does not replace them. | `results/a/` |
| e | two flags: `--settings gate` then `--settings op` | **(e) the last `--settings` wins:** only `op` fired ("GATE-op", `PINEAPPLE-op`, and no `gate.jsonl`). The four operator hooks still ran. An earlier flag's `hooks.Stop` is dropped, not merged. | `results/e/` |
| f | `{"hooks":{"Stop":[gate]},"disableAllHooks":true}` | **(f) `disableAllHooks` kills the gate** and every other hook ("Found 0 total hooks in registry", no hook log). It also hides the statusLine. The turn ends after `OK`. | `results/f/` |
| g | `{"hooks":{"Stop":[gate]},"allowManagedHooksOnly":true}` | **Ignored from `--settings`**: the gate and all 5 Stop hooks still ran. There is no managed-settings file on this machine, so the managed-source `allowManagedHooksOnly` path was **not measured**. Installing that file needs root. | `results/g/` |

What each result means for the native gate:

- The gate is viable. It is one `--settings` overlay that holds only `hooks.Stop`, and marion
  judges the review verdict inside the hook command.
- If the operator passes their own `--settings`, marion must merge it into its own overlay. Adding
  a second flag is not enough, because whichever flag comes last silently drops the other.
- A gate cannot share an overlay with `{"disableAllHooks":true}`. It is also dead when the
  operator's own settings set `disableAllHooks`. marion should detect that case and report the gate
  as Skipped rather than claim a review ran.
- Managed `allowManagedHooksOnly` is expected to disable the gate but has not been measured here.
  Treat it the same way as `disableAllHooks`.
- The hook input carries `session_id`, `transcript_path`, `cwd`, `last_assistant_message` and
  `stop_hook_active`. `stop_hook_active` is the loop guard. marion's round cap is still the
  authority.

Paths are scrubbed to `<scratch>`, `<tmp>` and `~`. The debug excerpts keep only the hook, plugin,
channel and cost lines.
