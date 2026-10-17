# marion's OS sandbox

## Canned and endpoint nodes

- **Who is sandboxed.** A node whose row states `Wrap` or `ReplaceOwn` (`HarnessSpec::os_sandbox`),
  on a canned or endpoint tree, on a host where the sandbox is on (`MARION_SANDBOX` is not `off`)
  and supported (`marion_harness::os_sandbox::support`). claude, opencode, copilot, goose, qwen
  and pi are wrapped; codex replaces its own Seatbelt (it cannot nest) with marion's and opens
  its thread `danger-full-access` inside it. cline, acp and agy are `Unsupported`, each with why.
- **What it may write.** Its agent dir, its working directory, its git worktree's admin dir (the
  one its registration under the operator's repository proves, never what its own `.git` names), its
  `TMPDIR`, and its row's measured paths (claude: `~/.claude/projects/<cwd key>` and
  `/tmp/claude-<uid>/<cwd key>`; goose: `~/.local/state/goose`, which a canned goose's relocated
  home already covers). Reads, exec, the network and the supervisor socket are unchanged.
- **What it may read of other nodes.** On macOS, nothing in another node's agent dir but its
  `contracts/` and `events.jsonl` (what a parent's bridge reads of its children), and the one tree
  it was spawned over (a child of a node is cut from its parent's worktree, which its bridge
  resolves to reach the supervisor): no other node's `config/node-token`, prompt or recording. Its
  own agent dir and every dir it may write stay readable. On macOS a dir it may write that holds
  marion's state dir (a checkout with the state dir inside it) grants nothing inside another
  node's agent dir: the sibling rules come after every grant, and nothing there is written either.
  On Linux, Landlock here restricts writes only, so these reads are not refused, and it can deny
  nothing inside a write grant: a sandboxed launch whose grants hold the state dir is refused, and
  says to set `MARION_STATE_DIR` to a directory outside the checkout.
- **What it is not kept from: acting as another node.** A node cannot read another node's files
  on macOS, apart from its contracts and event stream, but on both macOS and Linux it can read the
  environment of any process running as you, including another node's marion bridge, which holds
  that node's token for most harnesses. On Linux it can also read other nodes' files. Treat every
  node you run, in any project, as able to act as any other through marion.
- **What it may not write in its own agent dir.** Its `contracts/`, `events.jsonl` and
  `events.jsonl.1` are marion's record of it, which a parent's bridge and the operator's view
  read: on macOS the profile refuses the node's writes there (create, replace, remove, rename).
  On Linux Landlock grants whole directories, so these stay writable to the node there: a
  contract a parent's bridge reads back, or an event the view shows, is read as the node's own
  process tree last left it.
- **What it cannot change.** What the node may do and hand down (its tree's agent-types table, the
  scope its caller granted, the operator's opt-ins) is kept at `<state>/<project>/authority/<id>.json`,
  outside its agent dir, so rewriting its own agent dir widens nothing.
- **What a link it leaves grants.** Nothing. A node may move a directory it may write and leave a
  link in its place; on macOS that includes the root of each grant, its agent dir among them
  (Seatbelt lets a process rename the root of a `subpath` it may write; Landlock needs the right
  to remove from the parent, which no grant gives). So no grant is resolved through a component
  the node may write: each is its marion-owned ancestor resolved (the agents dir,
  `~/.claude/projects`, `/tmp/<prefix>-<uid>`, a checkout's parent) with the node's own names
  appended as written, and a launch refuses one that is a link. Seatbelt resolves a path when it
  is used, so a link the node swaps in while it runs resolves outside the grant and is refused;
  Landlock opens each rule through those names `O_NOFOLLOW`. A `TMPDIR` inside the agent dir is
  writable only as part of the agent dir. A checkout named through a link of its own is refused;
  a link above it is resolved.
- **marion's writes into its agent dir.** marion writes there outside any sandbox: config
  documents, contracts, the event stream, a root's change record and prompt, a pane's recording,
  `tmp/` and its git scratch. All of them go through one writer that opens the agent dir once and
  every directory below it `O_NOFOLLOW` from the one above, so a link the node left at a document
  is replaced and never written through, and a link or a file in place of the agent dir or a
  directory under it refuses the launch by name.
- **Its verification lines.** A spawn's `verification` lines run over the node's work under the
  node's own plan, with its `TMPDIR`, so a test script the child wrote is kept to what the child
  could write; the contract records them as `sandboxed`.
- **marion's own git in its tree.** A node may write its worktree's admin dir (git keeps its index
  there), so marion's git on a child tree reads the common dir from the operator's checkout, never
  from that dir's `commondir`, refuses a `config.worktree` holding more than sparse-checkout keys,
  and applies no attributes from the tree (`GIT_ATTR_SOURCE` = the empty tree,
  `core.attributesFile=/dev/null`): a `.gitattributes` there cannot pick one of the operator's
  filter, diff or merge drivers. A `git-lfs` file therefore reaches a child as its pointer.
  `GIT_ATTR_SOURCE` needs git 2.40; an older git still reads the tree's attributes. The system
  attributes file is skipped (`GIT_ATTR_NOSYSTEM`). git reads `<common-dir>/info/attributes`
  whatever these say, so while that file is non-empty and the repository's own config sets a
  filter, merge or diff driver program, marion runs no git in a child's tree and names both.
- **How.** `compile` attaches a `SandboxPlan` to the `Invocation`; `Invocation::command` runs the
  process under it: `sandbox-exec -p <profile> -D W0=… <program>` on macOS (paths only as
  parameters), Landlock ABI 2+ in `pre_exec` on Linux. A plan that cannot be prepared refuses the
  launch.
- **Containment.** `containment::on(t, Host)` counts marion's sandbox by the same decision, so a
  contained parent may start any child the sandbox also contains without the operator's opt-in.
- **Tests.** `os_sandbox` unit tests (the profile, the plan, a real process kept to its dirs, codex's
  replaced field) and `tests/it_live/os_sandbox_escape.rs` (a real child per row tries to write the
  operator's home). copilot denies its shell in a headless canned run, so its cell is ignored
  until a write-tool cell exists.

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

The honest limit: such a node runs unsandboxed as the operator's own uid, so it can read and write
everything the operator can, marion's state included (`operator.key`, every node's records). marion
treats it as the operator's equal, not as contained.
