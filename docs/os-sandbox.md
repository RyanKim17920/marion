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

## Linux

Run for real in an OrbStack container (Landlock ABI 8, aarch64): the Landlock unit test (a real
child kept to its dirs), the escape cells for claude, codex, opencode, pi and qwen, and five
cross_product cells all passed; `marion doctor` reports `Linux Landlock ABI 8`.

## Phase 2: the operator's own login

Built, with nothing admitted yet:

- **Row data.** A wrapped or replaced row carries a `Live` list: session and state directories in
  the operator's real home, and single files it rewrites (a credential a login refresh replaces, a
  history it appends to), each writable alone, never its directory. `admitted` names the passing
  admission run; until it is set the row does not cover `Inherited`, and its live nodes keep the
  containment their row has without the sandbox.
- **Expected lists** (from each harness's layout, to be confirmed by its run):
  - claude: `~/.claude/projects/<cwd key>`, `/tmp/claude-<uid>/<cwd key>`, `~/.claude/todos`,
    `~/.claude/statsig`, `~/.claude/shell-snapshots`, `~/.claude/session-env`; the credential file
    under `~/.claude/` a Linux login refreshes (macOS keeps it in the Keychain). Not
    `~/.claude.json`, which holds its MCP and project settings.
  - codex: `~/.codex/sessions`, `~/.codex/archived_sessions`, `~/.codex/log`; files
    `~/.codex/auth.json`, `~/.codex/history.jsonl`. Not `config.toml`.
  - opencode: `~/.local/share/opencode/storage`, `~/.local/share/opencode/log`,
    `~/.local/state/opencode`, `~/.cache/opencode`; file `~/.local/share/opencode/auth.json`.
  - pi: `~/.pi/agent/sessions`; file `~/.pi/agent/auth.json`.
  - gemini, copilot, goose, qwen: none stated yet.
- **An admission run** is `scripts/sandbox-admit.sh <harness>` (plan only) and then
  `MARION_SANDBOX_ADMIT_RUN=1 scripts/sandbox-admit.sh <harness>`: one short live task under the
  profile (`MARION_SANDBOX_ADMIT=1` puts the unadmitted row's node under it without counting it
  contained), on the operator's existing login, with a before/after `$HOME` mtime diff checked
  against `marion-supervisor sandbox-plan <harness>`. `scripts/sandbox-admit-apply.py <result>`
  turns a pass into the row's `admitted` note; a failed run, a missing `hello.txt` or a write
  outside the list is refused.
- **Known limit.** A credential refresh that writes a temporary file and renames it over the old
  one needs its directory writable, which a single-file grant refuses; the admission run shows
  whether a row's login refresh does that.

## Phase 2 notes still open


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
