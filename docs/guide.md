# marion guide

The reference behind the [README](../README.md): every verb, the home screen, logins and
profiles, where a child's work lands, and the per-harness detail. `marion --help` is the
authoritative list of verbs and flags; `MILESTONES.md` is the engineering log behind every claim
here.

- [Running and watching nodes](#running-and-watching-nodes)
- [The home screen](#the-home-screen)
- [Steering](#steering)
- [marion as an MCP server](#marion-as-an-mcp-server)
- [Where a child's work lands](#where-a-childs-work-lands)
- [Agent types](#agent-types)
- [Logins, API keys and endpoints](#logins-api-keys-and-endpoints)
- [Profiles: more than one login per harness](#profiles-more-than-one-login-per-harness)
- [Harness detail](#harness-detail)

## Running and watching nodes

```sh
marion run <agent-type> --prompt <text>   # watch it to the end in this terminal
marion run codex --prompt "…" --detach    # return once the root has started
marion run claude --prompt "…" --pane     # run in a terminal marion owns; attach to it later
marion run claude --prompt "…" --model haiku --timeout 600
marion ls                                 # the home screen on Watch; in a pipe, list's lines
marion ls <agent-id|short-id>             # one node's detail
marion list [--attention]                 # the forest once, one node per line
marion attach <agent-id>                  # a pane node's terminal; ^] d comes back
marion resume <agent-id> [--prompt <text>]
marion cancel <agent-id|short-id>         # recorded as cancelled; its children keep running
marion steer <agent-id|short-id> <text…>  # `-` reads the message from stdin
```

`--repo` defaults to the enclosing git repository, else the working directory. State
(journals, transcripts, sockets) lives under `--state-dir`, else `$MARION_STATE_DIR`, else
`$XDG_STATE_HOME/marion`, else `~/.local/state/marion`. Every verb follows that rule, so
`marion ls` shows a session only when it sees the same value the session started with.

`--timeout` is the root's bound: on claude, the budget for one blocked permission request; on
the other harnesses, a wall-clock limit, after which marion kills the node's process tree and
confirms it is gone.

`marion resume` survives the supervisor's own death: `kill -9` it, and `resume` relaunches the
lost root under the same id against the same harness session, recorded as a second generation.

**The first `marion claude` in a folder** shows Claude Code's own dialogs: whether you trust the
folder, then a one-time "Loading development channels" warning, because marion loads its channel
so that a background child's result reaches the session without a `wait`. `marion codex` asks its
own trust question. They are the harness's prompts, so you answer them; marion never does.
Inside `marion <harness>`, `^] d` detaches and leaves the session running (`marion attach
<agent-id>` brings it back) and `^] s` toggles marion's status row. Every other key goes to the
harness.

## The home screen

Bare `marion` on a terminal opens it; in a pipe it prints usage and exits non-zero. `Tab` changes
screen, `?` is help, `q` or `^c` quits. The box at the bottom shows the command each key runs.

- **Start** lists each harness's readiness as `marion doctor` finds it (● ready, ✗ broken with
  the fix, ○ not installed). `↑↓` picks the harness, `^o` its agent type (the plain name edits in
  a worktree; `<harness>-orchestrator` is read-only), `←→` a model you last ran it on, `^p`
  headless or pane. Type the task and press Enter: it runs `marion run … --detach`.
- **Watch** is the forest. `j/k` move, and the selected node opens in place with the task marion
  sent it, its steers, a live stream of its calls (`J/K` or Page Up/Down scroll), its tokens
  (never a price), its capabilities with the unmeasured ones greyed, and the branch it landed.
  Enter attaches, `s` steers, `x` cancels after asking, `u` resumes an ended node, `o` opens a
  shell in its workspace, `d` shows what its branch changed, `c` copies the merge command, `!`
  jumps to the next node that needs you.
- **Setup** shows the doctor checks with their fixes (`r` re-checks), the agent types (`e` edits
  `.marion/agents.toml` and validates it, `n` adds one) and your stored API keys (`a` adds one,
  `x` removes one after asking).
- **Help** lists the keys.

## Steering

A message for a running node goes into its inbox and reaches its model at the node's next turn
boundary, never mid-sentence. Whether that is a fold into the running turn, a paste, or the next
generation is per harness; see [Harness detail](#harness-detail).

- **A parent** steers with its `steer` tool, addressed by the `task_id` its `spawn` handle
  carries or by an `agent_id` from `list` (the only way to name a grandchild). A node may steer
  any node below it; its parent, siblings and itself are refused with one sentence. `wait` and
  `status` reach only its direct children. Before steering, a parent's `status` on a running
  child shows its last few tool calls and the last line it wrote.
- **The operator** presses `s` on a node in Watch, or runs `marion steer <id|short-id> <text…>`.
  Exit 0 prints the queued message's id; exit 1 prints the supervisor's refusal (an ended node
  points at `marion resume`).

## marion as an MCP server

`marion mcp` serves marion's tools over stdio for any MCP client: `spawn`, `wait`, `status`,
`list`, `steer`, and `report` (which answers only inside a child marion started, so a root sees
five). Its `spawn` creates a root over the same socket `marion run` uses, so the result shows up
in `marion ls` like any other. It is not a command to type at a terminal; configure a client with
it:

```sh
claude mcp add marion -- marion mcp --repo "$PWD"   # Claude Code
codex mcp add marion -- marion mcp --repo "$PWD"    # Codex
```

Any other client takes `command: "marion", args: ["mcp", "--repo", "/path/to/repo"]`.

## Where a child's work lands

A child spawned with `isolation: "worktree"` works in its own git worktree on the branch
`marion/<task_id>`. When it finishes, marion commits everything it changed onto that branch
(authored as your configured git user, or `marion <marion@localhost>` if none is set, with hooks
and signing skipped), removes the worktree directory, and keeps the branch. The contract records
the branch and commit (`completion.branch`, `completion.commit`), and `marion run` prints them
under the child's line. marion never merges into your branch:

```sh
git log -p HEAD..marion/<task_id>     # what the child did
git merge marion/<task_id>            # take it
git branch -D marion/<task_id>        # or drop it
```

Paths outside the child's `writable_scope` are committed too and listed in the contract's
`scope_violations`, so decide before merging. A child that changed nothing leaves its branch at
the commit it started from. If the commit fails, the worktree is kept and the contract says
where. A `shared-cwd` child writes straight into your directory, so there is nothing to merge.

## Agent types

An agent type is a launch spec, not a persona: harness, model, tools, isolation, prompt,
profile. A plain harness name (`claude`, `codex`, `opencode`, …) is that harness's implementer
with `read` and `write`; `<harness>-impl` is kept as an alias on every row except pi.
`<harness>-orchestrator` is the read-only planner, on the harnesses where marion can withhold
writes (not codex, opencode or cline). `acp-<row>` types reach the ACP agents below. `marion --help` lists
every built-in type; a repository adds its own in `.marion/agents.toml`.

Delegation stops at depth 3 (the root is depth 0): `max_depth` is a field of every agent type,
3 by default, and `.marion/agents.toml` does not set it yet. A node cannot exit while one of its
descendants is still running.

## Logins, API keys and endpoints

By default every node runs on the login you already set up for its harness: its OAuth session,
its API key variable, its keychain entry or its config file. marion writes no auth selection,
hides no credential source, and never runs a harness's login command.

`marion login` is only for keys and endpoints you give it:

```sh
marion login openrouter                  # read with echo off; or --stdin, or --from-env
marion login openrouter --label work     # several keys per provider: openrouter:work
marion login --list                      # providers and which keys are stored, never a key
marion login custom local --base-url http://127.0.0.1:11434/v1 --wire openai-chat --auth none
marion logout openrouter
marion run opencode --prompt "…" --model openrouter:qwen/qwen3-coder
```

A key goes into the macOS Keychain (service `marion`, passed to `security` on stdin, never argv)
or, elsewhere or with `MARION_CREDENTIAL_STORE=file`, into a `0600` file in a `0700` directory
under `$XDG_CONFIG_HOME/marion/`. It is never printed. `login custom` adds an OpenAI-, Anthropic-
or Gemini-compatible endpoint (`--wire anthropic|openai-chat|openai-responses|gemini`, comma
separated for several; `--auth none` for a keyless local server) to the user-level
`providers.toml`; a repository's `.marion/` is never read for providers. `providers.toml`'s
`[credentials]` table states the order a launch tries a provider's keys in. A provider prefix on
`--model` (endpoint mode) runs that node on the stored key.

**What marion never does with vendor accounts.** Subscription logins (Claude, ChatGPT/Codex,
Copilot, Google) stay inside their own harness. marion never reads, copies or reuses their OAuth
tokens, never presents itself as another client, and never rotates between subscription accounts
to get around a usage limit.

## Profiles: more than one login per harness

A profile is a directory you logged into with the harness's own login, so one node can run on
your work account and another on your personal one. marion never logs in, never copies, links or
reads a credential; it only points the harness at the directory.

```sh
marion profile add claude work              # makes the directory and prints the login to run yourself
marion profile add claude old --dir ~/.claude-work   # or adopt a directory you already use
marion profile list                         # logged in or out, last used, last limit reading
marion profile use claude work              # the default for claude nodes
marion profile remove old [--purge]         # --purge deletes only a directory marion made
marion run claude --prompt "…" --profile work
MARION_PROFILE=work marion claude           # a native session
```

Profiles live in `$XDG_CONFIG_HOME/marion/profiles.toml`, directories under
`$XDG_DATA_HOME/marion/profiles/<harness>/<name>/`. An agent type names one with
`profile = "work"`, or a list, `profile = ["work", "personal"]`. Resolution is the run's own
choice (`--profile`, or `MARION_PROFILE` for a native session), then the agent type, then
`[default]`, then the harness's own default login. An unknown profile, a profile of another
harness, or a directory that is gone is refused before anything starts.

What happens when a run fails depends on why:

- **Usage limit** (a 429, `usage_limit_exceeded`, "You've hit your session limit"): a notice
  leads the contract and the parent's `wait`, and the node needs attention in the tree. Nothing
  is relaunched and no other account is suggested.
- **Expired or refused login** (a 401, "Not logged in") on a child that reported nothing: marion
  relaunches it on the next profile its agent type listed and journals a `ProfileFailover`. A
  profile named at spawn, and a root, take that profile only.
- **Vendor outage** (a 529 or 5xx): recorded as the cause; the profile never changes.

A resume continues on the profile its session was recorded under. Profiles apply to live runs
only; `--canned` runs keep marion's own isolation.

| harness | profile variable | status probe | shared from the default directory |
|---|---|---|---|
| `claude` | `CLAUDE_CONFIG_DIR` (and `CLAUDE_SECURESTORAGE_CONFIG_DIR` removed) | `claude auth status --json` | `settings.json`, `CLAUDE.md`, `skills`, `agents`, `commands`, `keybindings.json` (links) |
| `codex` | `CODEX_HOME` | `codex login status` | `config.toml`, `AGENTS.md` (links; never `auth.json`) |
| `opencode` | `XDG_DATA_HOME` | `opencode/auth.json` exists | — |
| `gemini` | `GEMINI_CLI_HOME` | `.gemini/oauth_creds.json` exists | — |
| `pi` | `PI_CODING_AGENT_DIR` | `auth.json` exists | `settings.json` (link) |
| `copilot` | none: a fresh `COPILOT_HOME` still authenticates (1.0.83), so a directory cannot choose an account | | |
| `goose`, `cline`, `qwen`, `agy`, `acp:` | none yet: unmeasured | | |

## Harness detail

"Pin" is the oldest version whose evidence is on record, not a ceiling; the test suite admits
newer versions only after re-running against them (`scripts/admit-harness.sh`), and `marion
doctor --capabilities` measures whatever is installed at runtime.

| harness | admitted | headless shape | resume | a steer reaches it | native lane |
|---|---|---|---|---|---|
| `claude` | 2.1.220 – 2.1.283 | `-p` stream-json, or a pane | `--resume` | folded into the running turn | yes |
| `codex` | 0.146.0 – 0.155.1 | `exec --json`, or a pane | `exec resume` | next generation | yes |
| `opencode` | 1.17.3 – 1.18.32 | `run --format json` | `--session` | next generation (native: pasted) | yes |
| `pi` | 0.80.2 | `--mode rpc`; marion's tools through its own `-e` extension | `--session` | folded into the running turn | yes |
| `copilot` | 1.0.83 | `-p` | `--resume=` | next generation (native: pasted) | yes |
| `gemini` | 0.53.0 | `-p` | no | no | yes |
| `qwen` | 0.23.0 | `-p` stream-json | `--resume` | next generation | disabled: interactive surface unmeasured |
| `goose` | 1.49.0 – 1.52.0 | `run -t` | no | no | disabled: interactive surface unmeasured |
| `cline` | 3.0.61 | prompt + `--json` | no | no | disabled: interactive surface unmeasured |
| `agy` (Antigravity) | 1.2.8 | `-p` stream-json; needs your own `permissions.allow` rule for `mcp(marion/*)`, which marion never writes | `--conversation` | next generation | disabled |
| `acp:<command>` | n/a | the Agent Client Protocol | `session/load` where offered | per agent | — |

Known gaps: a pi child cannot call `spawn` (pi's `--tools` also governs extension tools); agy has
no canned route, so it is tested only live; `opencode run` names its session only once its first
response streams, so a node whose supervisor dies during its first request has nothing to resume.

### opencode against claude and codex

opencode is tested in all three of its shapes. Each `yes` is a test that drives the real binary
against the canned provider, or for `status` and usage a reader held to a real capture
(`MILESTONES.md`, s36):

| | `claude` | `codex` | `opencode` (`run`) | `acp-opencode` | `marion opencode` |
|---|---|---|---|---|---|
| child: spawn, report, worktree, verification, landed branch | yes (verification and branch untested) | yes | yes | yes (verification on a fake agent) | — (a root) |
| parent: `spawn` reaches marion, contract comes back | yes | yes | yes | yes | yes |
| a steer reaches it | mid-turn fold | next generation | next generation (`--session`) | mid-turn fold | pasted |
| a background child's end reaches it | next turn | next generation | next generation | next prompt | pasted |
| resumed after the supervisor is killed | unit-tested only | yes | yes | yes (`session/load`) | — |
| `status` shows its recent calls; usage read | yes | yes | yes (reasoning counted) | yes | — (no stream) |
| an expired child leaves no tool process | — | yes | yes | yes | — |
| marion's tools approved at launch | `--allowedTools` | declaration key | declaration key | answers the ask | declaration key |

### ACP agents

Any ACP agent runs through `acp:<command> [args…]`. These have a refinement row, probed on
2026-09-27 (S33, `tests/fixtures/s33-acp-agents/`) without an account marion made or a login
marion ran. "Opened" means `session/new` answered with a session; *provider* means it opened only
once pointed at a model provider through its own configuration. **Refused** rows got as far as
`initialize` and no further, and have no built-in type.

| row | command | built-in type | session/new | notes |
|---|---|---|---|---|
| `opencode` | `opencode acp` | `acp-opencode` | opened | tool call measured; canned recipe |
| `claude-acp` | `npx -y @agentclientprotocol/claude-agent-acp@0.66.0` | `acp-claude-acp` | opened (your claude login) | tool call measured; live file write 2026-09-27 |
| `codex-acp` | `codex-acp` | `acp-codex-acp` | opened (your codex login) | tool call measured; live file write 2026-09-27 |
| `copilot` | `copilot --acp` | `acp-copilot` | opened (your login) | bridge only through `--additional-mcp-config` |
| `kilo` | `kilo acp` | `acp-kilo` | opened, no account | a prompt needs a Kilo login or provider |
| `qwen` | `qwen --acp` | `acp-qwen` | opened, *provider* | turn run against a local endpoint; MCP tools deferred behind `tool_search` |
| `goose` | `goose acp` | `acp-goose` | opened, *provider* | turn run against a local endpoint |
| `fast-agent` | `fast-agent-acp -x` | `acp-fast-agent` | opened, *provider* | real bridge call against a local endpoint |
| `vibe` | `vibe-acp` | `acp-vibe` | opened, *provider* | no turn run |
| `vtcode` | `vtcode acp` | `acp-vtcode` | opened | needs `VT_ACP_ENABLED=1` or `[acp]` in its config; no turn run |
| `gemini` | `gemini --acp` | — | **refused** | Gemini Code Assist ineligibility |
| `auggie` | `auggie --acp` | — | **refused** | `auggie login` (account) |
| `qoder` | `qodercli --acp` | — | **refused** | `qodercli login` (account) |
| `cline` | `cline --acp` | — | **refused** | wants an ACP `authenticate` first |
| `pi-acp` | `pi-acp` | — | **refused** | shim/pi version skew |

`marion doctor --capabilities --harness acp` probes every row and names the MCP transports each
agent advertises; `marion-supervisor doctor --acp-command "<cmd>"` probes your own agent and says
when it turns out to be one of these rows.
