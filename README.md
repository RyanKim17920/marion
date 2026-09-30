# marion

Any model, any harness, as any subagent. A Claude Code session can delegate to a codex child,
and codex can delegate to any harness once you allow it. Each child runs in its own git worktree,
marion checks its work, and the result goes back to the parent as a structured contract, not as
prose.

<!-- DEMO PLACEHOLDER: a new recording of one real delegation (a Claude Code session spawns a
codex child, the child builds something in its worktree, the result returns and the branch is
merged) goes here. Do not put a screen tour here. -->
> **Demo:** a recording of a real delegation is on the way.

## What it does

- **Delegation by tool call.** A running agent gets marion's MCP tools (`spawn`, `wait`,
  `status`, `list`, `steer`, `report`), so it hands work to another harness by calling a tool,
  not by shelling out. The same tools but `report` work from any MCP client (`marion mcp`).
- **One tree, whatever the vendor.** Every node is journaled and addressable by id. `marion` opens a
  terminal UI over the whole tree: start a task, watch each node's live tool calls and tokens,
  steer it, attach to its terminal, take the branch it landed.
- **Your harness, unchanged.** `marion claude`, `marion codex`, `marion opencode`, `marion pi`, … run
  the harness's own TUI with your login, flags and keys, with marion's tools connected.
- **Results you can check.** A child's changes land on its own branch (`marion/433f-add-a-top-n`), committed and
  never merged for you. Its contract records the commit, its verification output, and any write
  outside its declared scope.
- **Rules enforced by marion, not by the prompt.** A node cannot exit while one of its children is running,
  delegation stops at depth 3 (each agent type's default), a timeout kills the node's whole
  process tree, a node can steer only nodes below it, and it can `wait` on or check the `status`
  of only its own direct children.
- **Survives a crash.** `kill -9` the supervisor, and `marion resume <id>` continues the root under the
  same id against the same harness session.

## Install

marion runs on macOS and Linux. On Windows, use WSL 2 and follow the Linux steps. You also need
`git` and at least one harness CLI on `PATH`, already logged in.

**Today, build from source** (Rust 1.94 or newer):

```sh
cargo install --locked --git https://github.com/RyanKim17920/marion marion-supervisor
marion --version
marion doctor
```

This installs `marion` and `marion-supervisor` side by side in `~/.cargo/bin`. Keep them
together: `marion` starts the supervisor next to it. marion never updates itself; to upgrade,
run the same command again.

**Prebuilt binaries: available from the first release.** No packaged release has been cut yet,
so none of these works today. Once one is, each installs the same archive (macOS and Linux,
glibc and static musl, arm64 and x86_64) with no Rust toolchain:

| channel (available from the first release) | command |
|---|---|
| shell installer | `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/RyanKim17920/marion/releases/latest/download/marion-supervisor-installer.sh \| sh` |
| Homebrew | `brew install RyanKim17920/tap/marion` |
| npm (Node 14.14+) | `npm install -g @ryankim17920/marion` |
| cargo-binstall | `cargo binstall --git https://github.com/RyanKim17920/marion marion-supervisor` |
| archives | the release page, one `.tar.xz` per target |

`marion doctor` checks both binaries, the state directory, git and every harness it finds, then
prints `overall: ready: yes` or `no`, with the fix for anything broken.

## Quickstart

**1. A free dry run, with no account and no cost.** marion's canned model server plays a Claude Code
root that delegates one edit to a codex child. You need `claude` and `codex` on `PATH`.

```sh
cargo install --locked --git https://github.com/RyanKim17920/marion marion-provider   # marion-canned
marion-canned &
MARION_CANNED=1 marion run claude --prompt "say hello"
```

**2. A real delegation on your own login.** This makes real model calls, which cost real money. Run it
inside a git repository:

```sh
marion run claude --model haiku --prompt "Spawn a codex child to create hello.txt containing hi, then tell me its branch."
```

When the child finishes, marion prints its branch under the child's line. Review it and take
it:

```sh
git log -p HEAD..marion/<branch>
git merge --no-ff marion/<branch>
```

**3. Work the way you already do.**

```sh
marion claude                 # Claude Code's own TUI, with marion's tools; ^] d detaches
marion                        # the home screen: Start a task, Watch the tree, Setup
marion run codex --prompt "…" --detach
marion ls                     # watch it
marion steer <id> "use the v2 API, not v1"
marion race --prompt "…" --on claude,codex --verify "cargo test"  # keep the seat that passes
marion export <id> -o run.html  # a shareable, secret-scrubbed report of a node and its subtree
```

**4. Or drive marion from any MCP client.**

```sh
claude mcp add marion -- marion mcp --repo "$PWD"
```

[docs/guide.md](docs/guide.md) covers every verb, the home screen's keys, steering, profiles,
endpoints and where work lands.

## Sharing a run

`marion export <id|short-id>` writes a report of a node and everything under it — the tree, each node's task, steers, a condensed timeline of what it ran, its checks, what it reported, the branch it landed and the tokens it spent — as Markdown for a pull request or issue, or with `-o report.html` as one self-contained page (inline style, no script, a CSP that fetches nothing). Keys behind the tree's endpoint nodes, secret-named environment values, secret-shaped strings (`sk-…`, `ghp_…`, bearer tokens, PEM keys, long hex) and your home directory are scrubbed from every string and again from the rendered file; the root's own prompt is left out unless `--include-prompt`, and `-o` writes the file `0600`. It reads the journal and contracts directly and starts no supervisor. [`docs/examples/run-report.md`](docs/examples/run-report.md) is what it writes for the test fixture (and [`run-report.html`](docs/examples/run-report.html) the page).

## Harnesses

| harness | native `marion <harness>` | tested against the canned provider | live runs on record |
|---|---|---|---|
| Claude Code (`claude`) | yes | yes | root, native session, ACP (2026-09-22, 2026-09-27) |
| Codex (`codex`) | yes | yes | root, child, ACP (2026-09-22, 2026-09-27) |
| opencode | yes | yes, in all three shapes | one ACP child on a free model (2026-09-22) |
| pi | yes | yes | none |
| Copilot CLI (`copilot`) | yes | yes | ACP child (2026-09-05), child (2026-09-22) |
| Gemini CLI (`gemini`) | yes | yes | none: blocked by Google for individual accounts |
| Qwen Code (`qwen`) | no | yes | none |
| goose | no | yes | none |
| Cline (`cline`) | no | yes | none |
| Antigravity (`agy`) | no | no canned route | one end-to-end run (2026-09-27) |
| any ACP agent (`acp:<command>`) | — | opencode's ACP mode | claude-acp, codex-acp (2026-09-27) |

Every row's launch shape was measured against a real install, and the test suite drives the
real harness binaries with only the model replaced. **Live coverage with paid models is thin.**
The last column lists every live run on record, so a green canned test means the plumbing
works, not that a vendor's endpoint accepts what marion sends. Gemini CLI fails with
`IneligibleTierError` on an individual account before it reaches the model, with or without
marion. Versions, steer and resume behaviour per harness, and the ACP agents table are in
[docs/guide.md](docs/guide.md#harness-detail).

## Logins

marion runs every harness on the login you already set up for it: its OAuth session, its API
key variable, or its config file. It never starts a login, never picks an auth method for you,
and never hides your credentials from the harness.

- **`marion key add <provider>`** stores an API key you give it (macOS Keychain, or a `0600` file)
  and adds custom OpenAI-, Anthropic- or Gemini-compatible endpoints, so any harness can run on
  any model through `--model <provider>:<model>`.
- **`marion profile`** points a harness at a directory you logged into yourself, so one node can
  use your work account and another your personal one.
- **Never:** marion never reuses a Claude, ChatGPT/Codex, Copilot or Google subscription token
  outside that vendor's own harness, never presents itself as another client, and never rotates
  between subscription accounts to get past a usage limit.

## Security and efficiency

- **The node's token never goes on argv.** Each node's bridge token reaches its harness through a
  declaration file or the environment. A lint rejects code that puts a secret on argv, and an
  end-to-end test reads the process table to confirm it.
- **Secrets are protected.** A stored key is never printed or logged; its type redacts `Debug`,
  and credential files are written `0600`. marion creates its state directory `0700`, and the
  supervisor's socket checks each caller's uid.
- **A node cannot act as you on the supervisor's socket.** Every connection first says who it
  speaks for: you, by presenting the key in `<state>/operator.key` (created `0600`, never handed
  to a node), or one node, by presenting that node's token. A node's connection may read the tree
  and steer, attach to or end only the nodes below it; starting a root, resuming, quitting and
  answering permission requests are yours alone. The key is a file of your own user, so a node
  running with no OS sandbox could still read it; the supervisor therefore also refuses the key
  from any process running inside a live node's process tree.
- **A runaway node meets a ceiling.** Every node starts with at most 8192 open files and a
  4096-process cap on your user (`RLIMIT_NPROC`), never raised above what you already have.
  Raise them under `[limits]` (`max_open_files`, `max_processes`) in
  `~/.config/marion/config.toml`.
- **A repository cannot redirect your keys.** Providers come only from your user-level config;
  a repository's `.marion/` is never read for them.
- **A repository cannot run its own command, or widen a node, without your consent.** A
  `.marion/agents.toml` row with `harness = "acp:<command>"`, or one that sets an
  authority-widening key (`approval_mode`, tools past `read`, `prompt_prefix`, `provider`,
  `credentials`, `profile`), runs only after `marion trust allow` has recorded the file's exact
  bytes, and any edit revokes that. There is never a prompt.
- **A model cannot run a program by naming it.** A `spawn` from a node or an MCP client may name
  `acp:<command>` only for a command you listed in `~/.config/marion/acp.toml`; your own
  `marion run acp:<command>` needs no listing.
- **Canned and endpoint nodes run in marion's own sandbox.** A node writes only its workspace,
  its `TMPDIR`, its agent dir and the few paths its harness measured it needs (macOS Seatbelt
  through `sandbox-exec`; Linux Landlock ABI 2 or later). Reads, commands and the network are
  unchanged. This covers claude, codex, gemini, opencode, copilot, goose, qwen and pi; cline,
  ACP agents and agy are not yet measured under it. Nodes on your own login keep running
  unsandboxed for now, because a harness writing its real home could plant a hook your next
  session runs. `marion doctor` says whether this host can sandbox nodes, and
  `MARION_SANDBOX=off` turns it off, which also drops the containment it gives.
- **An agent never starts one with more authority than its own.** marion compares the two on
  every axis before the child exists: a parent that cannot write or run commands cannot start
  one that can, a sandboxed agent cannot start one with no sandbox (codex is always sandboxed,
  and every row above is on a canned or endpoint tree; so a codex may start a claude child
  there, but not on your own login),
  a child cannot take an approval mode its parent lacks, and its writable scope stays inside its
  parent's. The built-in `*-orchestrator` planners are the one exception on writing: they write
  nothing themselves and exist to start implementers, still bounded by containment, scope and
  approval mode. Any other read-only agent starts only read-only ones — including a session you
  started in a read-only mode, such as `marion claude --permission-mode plan` or `marion codex
  --sandbox read-only`. marion sees only the flags you launched with: a mode you switch to inside
  the session (claude's shift-tab) or set in the harness's own config is invisible to it. The
  refusal names the axis and the fix: `marion run --allow-wider-children`, or `[delegation]
  allow_wider_children = true` in `~/.config/marion/config.toml`. Only you can opt in, and every
  child it lets through is journaled and marked on its row.
- **A repository cannot run its own command without your consent.** A `.marion/agents.toml`
  row with `harness = "acp:<command>"` runs only after `marion trust allow` has recorded the
  file's exact bytes, and any edit revokes that. There is never a prompt.
- **Near-zero idle cost.** marion waits on events, not timers. An idle supervisor with no
  running nodes measured 4 context switches per second, down from 264. The release binaries
  are 2.7 MB (`marion`) and 4.6 MB (`marion-supervisor`) on arm64 macOS.
  `scripts/perf-idle.sh` reproduces the measurement.

## Status

marion works end to end and is not finished. What is open:

- Live, paid coverage is thin (see above).
- Linux is less exercised than macOS, and native Windows is out of scope; see
  [CONTRIBUTING.md](CONTRIBUTING.md#windows).
- The native lane stays off for qwen, goose and cline until their interactive terminals are
  measured, and for agy, whose tools appear only after its first turn starts.
- A pi child cannot delegate further, because pi's `--tools` also governs extension tools.

## More

- [docs/guide.md](docs/guide.md): the usage reference.
- [CONTRIBUTING.md](CONTRIBUTING.md): build, test, the three-axis gate, releasing.
- [MILESTONES.md](MILESTONES.md): the engineering log, with every harness fact and the spike
  that measured it.
- [docs/](docs/README.md): the design and its specs.

MIT licensed.
