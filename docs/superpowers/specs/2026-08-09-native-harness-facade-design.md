# Native Harness Facade Design

Date: 2026-08-09

Status: revised direction; review pending

## Goal

For every supported interactive harness, invoking

```text
marion <harness> <that harness's normal arguments>
```

must feel like invoking the harness directly. Adding `marion` in front supplies one
supervised tree, durable process ownership, cross-harness subagents, recording,
detach/reattach, and Marion MCP tools. It must not replace the harness's CLI grammar,
settings, tools, permission model, TUI, or exit semantics.

The initial complete native facade set is:

```text
marion claude   <claude argv...>
marion codex    <codex argv...>
marion gemini   <gemini argv...>
marion opencode <opencode argv...>
```

The implementation is registry-driven. A future supported adapter gains a command by
declaring a native facade descriptor and injection implementation; central CLI parsing
does not gain another harness case.

ACP is a typed agent protocol, not an interactive PTY executable. ACP agents remain
fully supported as children and may later advertise typed facade descriptors, but are
not mislabeled as native TUI facades. Marion-on-Marion is an explicit recursive adapter
phase described below, not an accidental arbitrary-executable fallback.

The existing milestone and design ledgers remain the authority for sequencing and
completion. This design is the next vertical slice, not a replacement roadmap.

## Product principles

The facade is governed by three priorities:

1. **Maximum generality:** shared launch, PTY, lifecycle, and test infrastructure;
   adapter-owned injection only where harness protocols differ.
2. **Maximum compatibility:** opaque native arguments, normal user configuration,
   exact caller context, transparent terminal bytes, and hard version/live checks.
3. **Maximum ease of use:** no Marion flags after the harness selector; cwd and existing
   Marion configuration supply project/state defaults; one foreground command attaches,
   waits, and returns the harness result.

No supported facade may be advertised before its deterministic and pinned live TUI
cells pass. Unsupported is a named state, never a silently degraded managed launch.

## Command model

The following forms remain distinct:

```text
marion <facade-command> [opaque native argv]
marion run <agent-type> [Marion run arguments]
marion attach|tree|mcp|doctor [Marion arguments]
```

The first token selects either a reserved Marion command or an adapter facade. Once a
facade matches, every remaining OS argument belongs to that harness unchanged. This
includes:

- help/version flags;
- optional-value flags such as Claude resume;
- harness model, permission, cwd, sandbox, and configuration flags;
- names that collide with Marion flags (`--repo`, `--state-dir`, `--prompt`,
  `--timeout`, `--canned`, `--pane`, and `--live`);
- `--flag=value`, empty arguments, newlines, Unicode, non-UTF-8 values, the literal
  `--`, and everything after it.

Marion never maintains partial parsers for native harness CLIs. Unknown harness flags
are diagnosed by the harness.

The legacy `marion run` grammar and detached `run ... --pane` behavior stay compatible.

## Facade registry

Facade commands are neither agent-type names nor arbitrary executable names.

- Agent types include roles/personas such as `claude-impl`, aliases such as the current
  `codex` mapping, and ACP bindings; those are not user facade commands.
- Harness wire names include values such as `claude-code` and `acp`; those are not
  necessarily executable names.
- An installed executable alone never makes `marion <name>` valid.

Each registered adapter makes an explicit decision through this interface:

```text
NativeFacadeDescriptor
  command: FacadeCommandName
  aliases: [FacadeCommandName]
  executable: SupportedExecutableName
  root_agent_type: canonical AgentType name
  transport: TransparentPty | TypedAcp | RecursiveMarion

NativeFacadeAdapter
  descriptors() -> [NativeFacadeDescriptor]
  prepare_native(NativeNodeContext) -> NativeInjection
```

`prepare_native` never receives the user's argument tail. It returns only:

```text
NativeInjection
  argv_prefix: [opaque OS values]
  environment_overlay: [opaque key/value]
  documents: [temporary config documents]
  session_declaration: optional typed protocol data
```

The generic assembler alone constructs:

```text
resolved executable + fixed descriptor argv + injection prefix + exact user tail
```

This makes opacity structural: adapters cannot consume, reorder, validate, or UTF-8
decode native user arguments.

Adding a configured agent persona does not create a facade command. Adding a registered
adapter descriptor does. When the repository implements external adapter registration,
the same validated descriptor interface is its command-registration boundary.

### Command resolution

1. Collect `args_os`.
2. No arguments retains the existing picker.
3. The first token must be valid UTF-8 because it names a Marion command or facade; the
   tail remains opaque OS data.
4. Reserve `run`, `attach`, `tree`, `mcp`, and `doctor`, plus root Marion help/version.
5. Resolve every other token against the validated facade command/alias index.
6. Duplicate commands, command/alias collisions, alias collisions, and reserved-name
   claims are fatal registry errors naming both owners.
7. Unknown names return exit 2 with the reserved commands and currently supported facade
   commands. Marion never falls back to `PATH` execution.
8. Agent-type aliases never create facade aliases.

Help is generated from the registry. The no-argument picker is labeled as an agent-type
picker; it is not presented as a facade list.

## Shared native launch context

`AgentSpawnParams` gains an omitted-when-absent, root-only
`native_launch: NativeLaunchContextV1` field. V1 denies unknown fields; incompatible
changes create a new version instead of changing V1 silently.

```text
NativeLaunchContextV1
  facade_command: UTF-8 registry key
  platform: Unix | Windows
  argv: [OpaqueOsValue]
  program: OpaqueOsValue
  cwd: OpaqueOsValue
  environment: [NativeEnvVar]
  terminal: optional TerminalSize
  attach: TransparentForeground | TypedForeground
```

`OpaqueOsValue` is platform-tagged:

- Unix stores exact `OsStrExt::as_bytes()` units;
- Windows stores exact `encode_wide()` units.

The receiver rejects platform mismatch, embedded NUL, invalid environment names,
duplicate environment keys, oversized context, or values it cannot install. It never
uses lossy UTF-8.

The client resolves only the descriptor's declared executable through its current
`PATH`. It sends the resolved program, physical cwd, environment, and current terminal
geometry. Therefore a reused supervisor cannot substitute the environment of the shell
that happened to start it first.

The launch context is transient:

- accepted only from the same-UID root client;
- never written to journal, node events, contracts, logs, or formatted debug errors;
- client `MARION_*` identity variables are removed and replaced with supervisor-owned
  values;
- cwd is checked against repository/shared-cwd policy before side effects.

A native root also owns a transient `RootLaunchPolicy` in the supervisor. It carries
the facade's provider/auth choice as derived from the invoking environment and Marion's
existing project defaults, approved base endpoint, executable-search environment, and
non-secret adapter context used by descendants. The normal zero-configuration path is
inherited auth. Child spawns derive their launch context from the calling root instead
of the long-lived supervisor's stale environment. Secrets remain memory-only and
disappear with the existing node-token restart boundary.

Handler validation occurs before claim, journal, config, workspace, or process effects:

1. same-UID authorized root creation;
2. no caller/parent/task identifying a child request;
3. facade command resolves and matches requested root agent type and adapter binding;
4. adapter advertises the requested transport;
5. program matches descriptor resolution rather than arbitrary `PATH` input;
6. platform, bounds, NUL, environment, cwd, geometry, and provider policy validate;
7. only then claim and journal the node.

An old client remains byte-compatible with a new supervisor because the field is omitted.
A new facade client meeting an old strict supervisor fails loudly as “resident supervisor
does not support native facade V1”; it never downgrades to managed pane behavior.

## Adapter-owned native injection

Every interactive adapter adds Marion's MCP server and nothing else required by its
managed/headless policy.

| Facade | Program | Minimal injection | Managed flags/settings omitted |
| --- | --- | --- | --- |
| `claude` | `claude` | argv prefix `--allowedTools <Marion MCP names> --mcp-config <Marion JSON>` | `-p`, stream JSON, verbose, stdio permission tool, strict MCP, empty setting sources, built-in tool restriction, Marion model |
| `codex` | `codex` | global `-c mcp_servers.marion.*=<TOML>` entries, including bridge env and Marion-server approval | `exec --json`, repo skip, cwd/model, sandbox/approval, provider config, `CODEX_HOME`, plugin suppression |
| `gemini` | `gemini` | temporary system-settings overlay containing only trusted Marion MCP declaration | model/prompt/stream output, approval mode, config-home/auth relocation, forced trust/auth/telemetry/updater settings |
| `opencode` | `opencode` | `OPENCODE_CONFIG_CONTENT` containing only `mcp.marion` | `run --pure`, JSON/title/model/prompt, HOME/XDG relocation, instruction/skill/update/provider hygiene policy |

The prefix precedes the opaque tail so user `--` remains meaningful. Required variadic
injection values are terminated by another injected option before the user tail.

Normal user settings, plugins, hooks, built-in tools, MCP servers, authentication,
permissions, models, resume state, sandbox choices, and native flags remain active.
Only Marion MCP tools are pre-approved where the harness supports such a narrow setting.

Injection composes with an existing user-provided config overlay instead of overwriting
it. In particular, OpenCode's existing `OPENCODE_CONFIG_CONTENT` is parsed and merged
losslessly at the JSON-value level, and Gemini's existing system-settings source is part
of the merge gate below. An existing server with the reserved key `marion` is a named
conflict unless it is byte-equivalent to Marion's declaration. Claude/Codex conflict and
precedence behavior is measured against every accepted pinned version before release.

### Gemini merge gate

Gemini's system-settings merge granularity is not yet proven. Before its descriptor is
advertised, a pinned live, credential-free experiment must establish whether an MCP-only
system document deep-merges or replaces user/project `mcpServers` and unrelated settings.

- If it deep-merges, use the minimal overlay.
- If it replaces, build a transient effective document that preserves every user/project
  value and adds only Marion, with explicit duplicate-server refusal.
- If neither can preserve native behavior, Gemini remains named unsupported rather than
  shipping a compatibility claim that the test contradicts.

Equivalent configuration-conflict tests apply to all adapters. Marion does not grow a
speculative parser for undocumented settings precedence.

## ACP transport

ACP remains a distinct typed transport. Its programs include `opencode acp`,
`gemini --acp`, Claude's ACP shim, and `codex-acp`, but their stdin/stdout belong to the
ACP protocol, not a user's terminal.

ACP descriptors, when root support is implemented, use `TypedAcp` and inject Marion via
`session/new.mcpServers`. They do not use the transparent PTY view or claim normal TUI
behavior. Until root ACP lifecycle is implemented and tested, ACP stays on the exhaustive
child-adapter axis and no facade command is advertised.

## Transparent foreground transport

All interactive facade rows share one transparent PTY transport. The supervisor remains
sole owner of the harness, node PTY, process identity, journal, events, and contracts.
The invoking `marion` process is a foreground proxy; client detach/crash never kills the
node.

The flow is:

```text
registry facade selection
  -> NativeLaunchContextV1
  -> supervisor validation and adapter injection
  -> supervisor-owned controlling PTY/process
  -> atomic startup replay/live subscription
  -> transparent operator proxy
  -> lifecycle wait and native exit status
```

The client:

- enters raw termios without a Marion-owned alternate screen;
- does no grid parsing, repaint, CSI 3J suppression, mouse mirroring, OSC/image filtering,
  or terminal-query interception;
- writes node output directly to operator stdout;
- forwards operator input directly to the node;
- retains `Ctrl-] d` detach and `Ctrl-] Ctrl-]` literal escape;
- forwards initial and later geometry through the typed resize route.

General `marion attach` remains grid-based for bounded scrollback, multiple viewers, and
safe late replay.

The terminal guard has shared restoration with two policies:

```text
Grid        = raw termios + Marion alternate screen + grid cleanup
Transparent = raw termios + no entry bytes + conservative detach cleanup
```

Both restore exact termios and the prior panic hook. Transparent mode handles graceful
termination and suspend/resume, disables residual mouse modes, shows the cursor, and
leaves an active alternate screen when detaching. SIGKILL remains the unavoidable case
where the client cannot restore its terminal.

The operator's initial `TIOCGWINSZ` reaches the node PTY before `exec`, so the first TUI
frame is not laid out at 80x24. Non-TTY callers proxy available streams with fallback
geometry; full TUI parity is guaranteed only for TTY callers. Marion never parses the
opaque tail to guess whether a native command is interactive.

## Exact PTY bytes and startup splice

Exact bytes are additive and negotiated:

- pane attach advertises `exact_pty_bytes`;
- `NodePty` and `NodePtyWrite` retain legacy UTF-8 text and gain optional base64 exact
  data;
- valid UTF-8 stays compact; invalid segments include the legacy lossy fallback plus
  exact bytes for negotiated raw clients;
- malformed base64 or conflicting representations are named protocol errors;
- asciicast `o`/`i` remain valid for UTF-8, with distinct extension codes for base64
  non-UTF-8 input/output.

`PtyHost::subscribe_with_replay` establishes one ordering seam:

1. capture durable startup frames and the last included sequence;
2. install a paused live listener at the next sequence;
3. return replay, geometry, byte capability, and first-live sequence;
4. queue new frames without sending them before the response;
5. activate only after the attach response is written.

The first facade client applies startup replay once, then requires dense live sequences.
Gap, duplicate, or regression is fatal. This covers fast help/version/parse-error exits.
Late general attach uses the safe grid replay rather than replaying historical terminal
side effects directly into a real terminal.

## Foreground lifecycle

`marion <facade>` performs spawn, immediate transparent attach, lifecycle wait, terminal
restoration, and result propagation as one command.

- Normal harness exit returns its code.
- Signal exit records the signal and returns the platform shell convention.
- User detach restores the terminal, leaves the node running, and prints the durable ID
  and reattach command.
- Transport failure is a Marion error and never asserts that the harness exited.
- Fast exit output/status comes from startup replay and terminal lifecycle rather than a
  misleading spawn-success zero.

## Marion-on-Marion

Marion becomes a supported facade only through an explicit `Harness::Marion` adapter,
canonical agent type, and descriptor:

```text
command = marion
executable = marion
transport = RecursiveMarion
```

The outer supervisor still owns the inner Marion process. The adapter injects a scoped
parent-node endpoint/identity. When the inner CLI launches a facade, it recognizes this
nested context and sends `agent/spawn` as a child of the outer node instead of starting
another top-level supervisor. Client-supplied `MARION_*` values are scrubbed, and only
the scoped parent identity is accepted.

Acceptance is:

```text
marion marion codex <native codex argv>
```

produces one supervisor tree with a Marion root and Codex child, preserves the inner
Codex native TUI through the nested foreground proxy, and allows that Codex child to
spawn another harness. No recursion-name special case belongs in the CLI parser; this
is adapter and nested-client behavior.

Until that adapter and same-tree invariant pass, `marion` is not advertised as a facade.

## Deterministic cross-harness matrix

Add:

```text
crates/marion-supervisor/tests/native_facade_matrix.rs
crates/marion-supervisor/tests/helpers/native_facade_probe.rs
```

Rows are enumerated from advertised `TransparentPty` facade descriptors. Columns are
enumerated exhaustively from production child adapters:

```text
facade roots: claude | codex | gemini | opencode
child adapters: claude-code | codex | gemini | opencode | acp-opencode
```

The initial required matrix is 4×5 = 20 cells. Cardinality is asserted from the registries,
so a new facade or child adapter creates a failing uncovered cell.

A checked-in Rust probe is compiled once and installed under each descriptor's program
name on an isolated `PATH`. It reports exact OS argv, selected environment/cwd, all three
TTY states, initial geometry, terminal sequences, input, resize/SIGWINCH, and exit 37.
It reads each adapter's real Marion injection, starts the declared MCP bridge, performs
initialize/tools-list, calls `spawn`, and observes the resulting child through a separate
tree subscription.

Causal barriers—not sleeps—prove:

1. probe control socket ready;
2. supervisor-owned root exec observed;
3. startup marker durable in cast;
4. startup replay visible in operator PTY;
5. live marker arrives at the next sequence;
6. injected MCP bridge initialized and listed tools;
7. cross-harness child spawn returned a handle;
8. tree and journal show root→child and durable `Spawned` PID;
9. far-side input and exact resize arrive;
10. exit 37 restores terminal and becomes Marion exit 37.

Per cell, assert journal/events/cast, supervisor identity, process ownership, sanitized
MCP transcript, no secret-shaped sentinel persistence, and zero surviving processes.
Runner deadlines diagnose deadlock only; elapsed time never establishes success.

No deterministic cell may skip because a vendor CLI is absent; probes supply the programs.
The current ACP test's missing-OpenCode skip is replaced with fail-loud version handling
in its separate real cell.

## Pinned live compatibility

Add one independent real TUI cell per advertised facade. Each uses the accepted pinned
binary, isolated config/home, loopback canned provider, dummy credentials, and a dead
external proxy. It submits no paid prompt.

| Command | Native arguments exercised | Required live oracle before detach |
| --- | --- | --- |
| `marion claude` | model plus a valid native permission mode | real trust/composer UI and Marion MCP ready |
| `marion codex` | native model and cwd flags | real Codex composer and Marion MCP ready |
| `marion gemini` | native model/approval flags | real Gemini composer, merge preservation, Marion MCP ready |
| `marion opencode` | native model flag | real OpenCode composer and Marion MCP ready |

Every cell asserts initial geometry, one live resize, transparent terminal protocols,
detach restoration, root survival, successful reattach, zero billable model requests,
and zero non-loopback traffic. Missing or unaccepted versions fail; they never skip.

Live CLI compatibility and the deterministic matrix are separate evidence. A probe cannot
certify a vendor's current CLI, and a live composer cannot prove opaque argv or every
cross-harness child pairing.

## Dogfood evidence

Two local $0 cells are required:

1. `marion codex` runs on the Marion repository with real pinned Codex and a canned
   provider; Codex spawns and waits for real OpenCode through ACP. The contract returns
   through the same outer tree, all traffic remains loopback, and the operator checkout's
   porcelain status is byte-identical before/after.
2. After the recursive adapter lands, `marion marion codex` proves one supervisor,
   Marion root→Codex child→another cross-harness child, native nested TUI, contracts, and
   zero checkout mutation.

## Mutation requirements

The slice is not complete until independent mutations are killed for the intended reason:

| Mutation | Required failing evidence |
| --- | --- |
| Hardcode or delete a facade command | registry resolution/cardinality |
| Treat agent aliases or arbitrary PATH names as facades | collision/unknown tests |
| Consume, reorder, UTF-8 decode, or drop opaque tail values | indexed raw argv equality |
| Resolve with the supervisor's stale PATH/env/cwd | launch-context probe |
| Apply one adapter's injection to another | 4×5 matrix row |
| Carry managed policy into native mode | adapter injection/native config assertions |
| Clobber user configuration | pinned merge-preservation cell |
| Omit node identity or child policy | MCP spawn/tree relationship |
| Spawn from the client instead of supervisor | writer/PID/journal ownership |
| Use grid rendering in facade mode | raw terminal-sequence assertion |
| Subscribe live-only or deliver before response | startup replay/dense sequence |
| Lose invalid bytes | exact input/output assertions |
| Resize only the client | far-side SIGWINCH/kernel geometry |
| Swallow child exit | expected facade exit 37 |
| Detach by cancelling root | survival/reattach assertion |
| Weaken a live version failure to a skip | no-skip latch |
| Start a second supervisor in recursive mode | one-tree dogfood assertion |

## Targeted complexity constraints

Shared facade transport, context, registry, and test enumeration live in shared modules.
Adapter-specific injection remains in adapter modules. The CLI owns no harness match.

Two behavior-neutral dependency moves accompany the work:

1. move `Auth` from the adapter hub to a leaf `auth` module;
2. move `ROOT_DEPTH` from `root` to a neutral lifecycle/depth leaf.

No broader supervisor/registry/TUI reorganization is part of this slice. Sentrux's
pre-change quality signal is 5146 with acyclicity as the bottleneck. Session-end scanning
is a structural non-regression signal, never a substitute for behavior evidence.

## Implementation order

1. Add registry descriptors/resolution and opaque versioned launch types.
2. Add shared transient root launch policy and strict pre-effect validation.
3. Add generic native assembler and the four adapter injection implementations.
4. Run the Gemini merge experiment and gate its descriptor on truthful preservation.
5. Add pre-exec geometry, exact byte negotiation, and compatible cast extensions.
6. Add atomic replay/live subscription and transparent terminal/lifecycle view.
7. Add the deterministic 4×5 matrix and pinned four-row live TUI target.
8. Add Marion-repository Codex→ACP dogfood.
9. Add explicit Marion recursive adapter/nested-client mode and its dogfood test.
10. Apply the two cycle-breaking leaf moves.
11. Run targeted, milestone, workspace, mutation, live, and Sentrux verification.

Changes land as small logical commits. Each step compiles and keeps its affected tests
green before the next step.

## Non-goals

- Executing arbitrary first-token programs from `PATH`.
- Turning every agent persona or ACP binding into a native TUI command.
- Parsing any harness's evolving native CLI.
- Replacing legacy `marion run` or general grid attach.
- Advertising a facade before its adapter and live compatibility evidence exist.
- Treating external adapter/config loading as complete merely because the registry seam
  can support it later.
- Claiming byte-exact or recursive behavior on a platform without the matching tests.

## Acceptance criteria

This slice is complete only when:

- Claude, Codex, Gemini, and OpenCode are all advertised from adapter descriptors and
  accept their opaque normal argv after `marion <command>`;
- shared client/supervisor/PTy code contains no harness-specific dispatch;
- each adapter injects only Marion connectivity while preserving native user behavior;
- cwd, transient environment/provider policy, initial size, exact input/output, resize,
  detach/reattach, child spawning, and real exit semantics cross the shipped path;
- startup output cannot be lost or duplicated;
- the deterministic 20-cell matrix, four pinned live TUI cells, Marion-repository
  dogfood, and recursive Marion dogfood pass without paid traffic or silent skips;
- existing managed run, attach, protocol, M1/M2/M3/M4/M5 evidence, fmt, clippy, and full
  workspace targets remain green;
- every listed mutation fails for the intended assertion and Sentrux reports no
  structural regression.
