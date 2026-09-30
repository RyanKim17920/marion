# marion's OS sandbox

## Phase 1 (this branch)

- **Who is sandboxed.** A node whose row states `Wrap` or `ReplaceOwn` (`HarnessSpec::os_sandbox`),
  on a canned or endpoint tree, on a host where the sandbox is on (`MARION_SANDBOX` is not `off`)
  and supported (`marion_harness::os_sandbox::support`). claude, gemini, opencode, copilot, goose,
  qwen and pi are wrapped; codex replaces its own Seatbelt (it cannot nest) with marion's and opens
  its thread `danger-full-access` inside it. cline, acp and agy are `Unsupported`, each with why.
- **What it may write.** Its agent dir, its working directory, its git worktree's admin dir, its
  `TMPDIR`, and its row's measured paths (claude: `~/.claude/projects/<cwd key>` and
  `/tmp/claude-<uid>/<cwd key>`; goose: `~/.local/state/goose`, which a canned goose's relocated
  home already covers). Reads, exec, the network and the supervisor socket are unchanged.
- **How.** `compile` attaches a `SandboxPlan` to the `Invocation`; `Invocation::command` runs the
  process under it: `sandbox-exec -p <profile> -D W0=… <program>` on macOS (paths only as
  parameters), Landlock ABI 2+ in `pre_exec` on Linux. A plan that cannot be prepared refuses the
  launch.
- **Containment.** `containment::on(t, Host)` counts marion's sandbox by the same decision, so a
  contained parent may start any child the sandbox also contains without the operator's opt-in.
- **Tests.** `os_sandbox` unit tests (the profile, the plan, a real process kept to its dirs, codex's
  replaced field) and `tests/os_sandbox_escape.rs` (a real child per row tries to write the
  operator's home). gemini and copilot deny their shell in a headless canned run, so their cells
  are ignored until a write-tool cell exists.

## Phase 2 (design notes only)

Goal: the same containment on the operator's own login (`Auth::Inherited`), where a harness uses
its real home.

1. **Narrowed writable homes, per row, as data.** A new `WritePath` set used only under
   `Inherited`, naming session and state subpaths, never a harness's settings, hooks, agents or
   skills: claude `~/.claude/projects/<cwd key>` plus its todo/statsig state; codex
   `~/.codex/sessions` and `~/.codex/log`; gemini `~/.gemini/tmp/<project hash>`; opencode
   `~/.local/share/opencode/storage` and its log dir; pi `~/.pi/agent/sessions`; qwen
   `~/.qwen/tmp`; copilot `~/.copilot/session-state`; goose `~/.local/state/goose` and its
   sessions dir. A row whose writes cannot be narrowed that far stays `Uncontained` under
   `Inherited`.
2. **One live admission run per row**, with the row's no-self-update switch, under the operator's
   login: a turn, a resume, and a shell command. Pass criterion: the same outcome as unsandboxed,
   and a file-mtime diff of `$HOME` showing writes only under the narrowed paths. Recorded as the
   row's verified sandbox, beside its verified harness version, and re-run when the version gate
   admits a new one.
3. **Credential stores.** Keychain access is a Mach service, not a file write, so it keeps working;
   file-backed logins (`~/.codex/auth.json`, gemini's oauth file) are read, and a token refresh
   rewrites them: each such file is a narrowed `literal` write, never its directory.
4. **State-root read denial** (from the phase 1 design): deny reading `<state>` except the node's own
   agent dir, once the bridge gets a child's contract over the socket instead of from disk.
5. **The remaining rows.** cline and each ACP agent get their own measurement; agy stays
   `Unsupported` until a live run shows its tools execute inside the launched tree.
6. **gemini and copilot escape cells** through their write tools, or through their shell with the
   approval their headless mode needs made explicit in the row, measured first.
