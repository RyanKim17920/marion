# marion's OS sandbox

## Canned and endpoint nodes

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
  replaced field) and `tests/it_live/os_sandbox_escape.rs` (a real child per row tries to write the
  operator's home). gemini and copilot deny their shell in a headless canned run, so their cells
  are ignored until a write-tool cell exists.

## Linux

Run for real in an OrbStack container (Landlock ABI 8, aarch64): the Landlock unit test (a real
child kept to its dirs), the escape cells for claude, codex, opencode, pi and qwen, and five
cross_product cells all passed; `marion doctor` reports `Linux Landlock ABI 8`.

## The operator's own login

Decided by the owner: on the operator's own login (`Auth::Inherited`) a node runs exactly as its
harness normally does, in its own permission or auto mode, with no marion sandbox layered on.
marion's sandbox is for canned and endpoint nodes, where marion owns the environment and the
harness home. Containment on the operator's login is unchanged: only codex counts as sandboxed,
by its own sandbox, so the delegation rules there are as before (`marion doctor` and the guide
say so).
