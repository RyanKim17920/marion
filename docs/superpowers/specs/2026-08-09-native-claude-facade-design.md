# Native Claude Facade Design

Date: 2026-08-09

Status: approved direction; implementation pending

## Goal

`marion claude <claude-argv...>` must feel like launching `claude
<claude-argv...>` in the foreground. Claude owns its command-line grammar, settings,
tools, hooks, permissions, terminal modes, and exit status. Marion adds supervision,
durable identity, child-agent MCP tools, recording, detach/reattach, and process
accounting without turning Claude's TUI into a different application.

This is a targeted product slice. The existing milestone and design ledgers remain the
authority for later work. This slice does not replace `marion run`, redesign the
supervisor, or introduce client-owned roots.

## Product contract

The following invocations are distinct:

```text
marion claude [any Claude arguments]
marion run <agent-type> [Marion arguments]
```

For the first form, every OS argument after the literal `claude` belongs to Claude,
without parsing or reinterpretation by Marion. This includes:

- `--help`, `-h`, `--version`, and `-v`;
- `--model`, `--permission-mode`, `--resume`, `--continue`, and `--add-dir`;
- flags whose names collide with Marion flags, including `--repo`, `--state-dir`,
  `--timeout`, `--prompt`, `--canned`, `--pane`, and `--live`;
- `--flag=value`, empty arguments, newlines, Unicode, non-UTF-8 OS arguments, and
  the literal `--` plus everything after it.

Marion does not maintain a second Claude parser. It neither validates optional Claude
flag values nor invents precedence between Claude flags.

The legacy `marion run` grammar and detached `marion run ... --pane` behavior remain
unchanged.

## Architectural boundary

The supervisor remains the sole owner of Claude, the node PTY, process identity, event
stream, contract, and journal records. The invoking `marion` process is a foreground,
transparent PTY client. Killing or detaching that client does not kill Claude.

The new flow is:

```text
client argv/cwd/env/geometry
        |
        v
marion claude dispatch
        |
        v
root-only NativeLaunchContext over agent/spawn
        |
        v
supervisor validation -> Claude native-pane compilation
        |
        v
supervisor-owned PTY + Claude process
        |
        v
atomic startup replay/live subscription
        |
        v
transparent foreground proxy -> wait -> real exit status
```

No client-written journal records, external-root reservation protocol, inherited
terminal file-descriptor passing, or client-owned node token is introduced.

## Command dispatch

`marion` collects arguments with `std::env::args_os`, not `std::env::args`.

Dispatch order is:

1. An exact first argument of `claude` selects the native facade and captures the
   remaining `OsString` values unchanged.
2. Existing Marion verbs (`attach`, `tree`, and `mcp`) retain their parsers.
3. Marion help processing and the legacy `run` parser retain their existing behavior.

This ordering ensures `marion claude --help` reaches Claude while `marion --help`
continues to show Marion help.

The facade obtains its repository/state defaults from the invoking cwd and existing
Marion environment/configuration. Apparent Marion flags in Claude's tail are never
consumed.

## Native launch context

`AgentSpawnParams` gains an optional, root-only `native_launch` field. An absent field
retains the current serialized shape. The value contains:

```text
NativeLaunchContext
  argv: Vec<OpaqueOsValue>
  program: OpaqueOsValue
  cwd: OpaqueOsValue
  environment: Vec<NativeEnvVar>
  terminal: Option<TerminalSize>
  attach: TransparentForeground
```

`OpaqueOsValue` has an explicit platform-tagged representation:

- Unix: the exact `OsStrExt::as_bytes()` byte vector;
- Windows: the exact `encode_wide()` `u16` vector.

The receiver refuses a platform mismatch. It never decodes through lossy UTF-8.
Environment keys and values use the same representation. Invalid names, embedded NUL,
oversized frames, or an environment that cannot be installed are named refusals; none
are silently dropped.

The client resolves the Claude executable using its current `PATH` and supplies the
resolved program. It also sends its current environment and physical cwd so a reused
supervisor cannot substitute a prior shell's executable, proxy configuration, locale,
or hooks. This context is transient:

- it is accepted only from the same-UID authorized root client;
- it is never written to the project journal, node event stream, contracts, logs, or
  error debug output;
- supervisor-owned `MARION_*` identity values override and remove any client-supplied
  values with those reserved names;
- cwd is checked against the selected repository/shared-cwd policy before any journal
  or filesystem side effect.

Validation order at the handler boundary is:

1. caller is an authorized root creation;
2. requested agent type is exactly the built-in Claude facade type;
3. pane/native surfaces are available;
4. no parent/caller/task identifies this as a child spawn;
5. program, cwd, environment, arguments, and geometry satisfy wire and containment
   bounds;
6. only then claim a node and append spawn intent.

Nonempty native launch data on a child, another harness, or a non-pane root is refused
before journal, state-directory, worktree, or process side effects.

## Claude invocation

The native Claude compiler is separate from the existing headless and managed-pane
compilers. Its argv is:

```text
<resolved claude program>
  --allowedTools <Marion MCP tool names>
  --mcp-config <generated Marion MCP config>
  <exact user argv tail>
```

The Marion prefix appears before the opaque tail so a user `--` remains meaningful.
The `--allowedTools` value is one comma-separated argument followed by another option,
so Claude's variadic parser cannot consume the user's first positional argument.

The native compiler does not add:

- `-p`, stream-json input/output, or `--verbose`;
- `--strict-mcp-config`;
- `--setting-sources ""`;
- `--tools`;
- a structured `--model`;
- Marion's stdio permission-prompt tool.

Therefore normal Claude settings, plugins, hooks, built-in tools, MCP servers,
permission dialogs, model selection, resume behavior, and user flags remain active.
Only Marion's own MCP tools are pre-approved. The user may still choose stricter Claude
permission flags in the opaque tail.

Marion does not parse or rewrite the user's other MCP configuration. A user/project
server with the reserved name `marion` remains an explicitly documented incompatibility
until Claude exposes a supported conflict-discovery interface; this slice does not grow
a speculative settings parser or claim undocumented server-precedence behavior.

The existing headless and managed-pane compilers are unchanged.

## Initial terminal and process launch

When the invoking client has a terminal, it captures `TIOCGWINSZ` before spawning and
places that geometry in `NativeLaunchContext`. The supervisor applies it to the PTY
before `exec`, so Claude's first layout is not rendered at the current 80x24 default and
then resized.

Claude still receives a real controlling PTY on all three standard descriptors, its
own session/process group, Marion's supported `TERM`, and the existing spawn barrier.
The foreground client attaches immediately after the supervisor returns the node ID.

If no terminal geometry is available, launch still proxies available input/output and
uses the existing fallback PTY geometry without entering operator raw mode. Full TUI
parity is guaranteed only for a TTY caller. Marion does not inspect Claude's opaque argv
to guess whether a command is interactive.

## Transparent foreground view

The direct facade uses a transparent view. General `marion attach` continues to use the
grid renderer.

Transparent view behavior:

- set the operator terminal to raw mode;
- do not enter a Marion-owned alternate screen;
- do not parse or repaint through `marion-term`;
- do not suppress CSI 3J, OSC, synchronized-output, image, mouse, cursor, or terminal
  query sequences;
- write Claude's PTY output bytes directly to operator stdout;
- forward operator input bytes directly to the node PTY;
- retain `Ctrl-] d` as the documented detach chord and `Ctrl-] Ctrl-]` as the literal
  escape;
- forward initial and later terminal geometry through the existing typed resize path;
- leave Claude alive when this client detaches or dies.

The terminal guard is split into two policies sharing one restoration core:

```text
Grid        = raw termios + Marion alternate screen + current grid cleanup
Transparent = raw termios + no entry bytes + conservative detach cleanup
```

Both policies restore the exact saved termios and prior panic hook. Transparent cleanup
disables mouse modes, shows the cursor, and leaves an active alternate screen only when
detaching or unwinding; on ordinary Claude exit, Claude's own cleanup runs first and the
extra inverse sequence is idempotent. Graceful SIGTERM, SIGHUP, suspend/resume, normal
drop, and panic restore the terminal. SIGKILL cannot run process cleanup and remains an
OS limitation.

## Exact PTY bytes and compatibility

The existing protocol retains its UTF-8 `bytes` field. Exact byte support is additive
and negotiated by pane attach:

- `PaneAttach` advertises `exact_pty_bytes`;
- `NodePty` and `NodePtyWrite` gain an optional base64 exact-byte field;
- valid UTF-8 remains in the compact legacy field;
- invalid byte segments carry the legacy lossy fallback plus the exact field for a
  negotiated raw client;
- the handler requires exactly one authoritative input representation and refuses
  malformed base64 or disagreement.

Legacy clients and existing JSON fixtures continue to work. The transparent client
reconstructs exact bytes, while the grid client keeps consuming text.

Asciicast v3 `o` and `i` records remain unchanged for valid UTF-8. Marion adds distinct
extension codes for non-UTF-8 output and input whose payload is base64. Updated readers
accept both forms, and old readers safely ignore unknown extension codes. Resize and exit
records are unchanged.

## Atomic startup replay and live splice

The native facade must not lose output between process start and attachment, including
fast `--help`, `--version`, and parse-error exits.

`PtyHost` gains one deep `subscribe_with_replay` operation that, under the stream's
ordering boundary:

1. captures the durable cast prefix and last included sequence;
2. installs a paused live listener at the next sequence;
3. returns replay frames, current geometry, exact-byte capability, and first-live
   sequence;
4. queues new live frames without delivering them ahead of the attach response;
5. activates delivery only after the response has been written.

The client applies the replay once, verifies the first live sequence, and then forwards
live bytes. Duplicate, regressing, or gapped sequences are fatal named protocol errors;
they are never papered over by repainting.

Raw replay is used only for the first foreground client that never observed those
bytes. Late ordinary `marion attach` uses the safe grid replay plan, because replaying
historical bells, clipboard writes, images, and resize side effects directly into a real
terminal is not generally safe.

## Foreground lifecycle and exit status

`marion claude` is a single foreground command:

1. create the supervised root;
2. attach transparently;
3. remain attached until Claude exits, the user detaches, or the connection fails;
4. wait for the node's terminal lifecycle result;
5. restore the operator terminal;
6. return Claude's exit semantics.

Normal exit returns Claude's code. Signal exit uses the platform shell convention
`128 + signal` after recording the actual signal. A user detach returns success after
terminal restoration and prints the durable agent ID plus reattach command. A client
transport failure returns a Marion error without asserting that Claude exited.

Fast exits are recovered from the atomic replay and terminal lifecycle stream, so the
facade never reports spawn success merely because Claude ended before attach.

## Error and security rules

- Unknown Claude flags are Claude's errors, not Marion usage errors.
- Marion errors are limited to facade setup, authorization, containment, transport,
  terminal, and supervision failures.
- Opaque argv and environment values are never formatted in full error/debug output.
- Raw terminal output is user-visible by definition but is not copied into supervisor
  logs.
- The existing same-UID root authorization boundary remains unchanged.
- Client-provided program/environment can select code only for that client's authorized
  root. It cannot affect child spawns or another root.
- Node identity, MCP credentials, and reserved Marion variables always come from the
  supervisor.
- A detach or client crash never becomes implicit cancellation.

## Deterministic end-to-end evidence

Add a focused integration target and checked-in helper source:

```text
crates/marion-supervisor/tests/claude_passthrough.rs
crates/marion-supervisor/tests/helpers/native_tui_probe.rs
```

The test compiles the probe with the pinned Rust compiler into an isolated `PATH` as
`claude`. A test-owned Unix control socket provides length-prefixed binary rendezvous
frames. No elapsed-time threshold is a passing assertion.

The probe reports:

- exact raw `args_os` entries and boundaries;
- whether stdin/stdout/stderr are terminals;
- selected environment/cwd and initial `TIOCGWINSZ`;
- output committed before attach and output emitted live;
- exact input bytes received;
- `SIGWINCH` and post-resize kernel geometry;
- a deliberate final exit code of 37.

The primary test invokes the compiled `marion` binary with a collision-heavy argv tail
containing help/version, model, resume, Marion-looking flags, empty/newline/Unicode and
non-UTF-8 arguments, `--`, and literals afterward. It asserts:

- the exact user tail survives client, protocol, handler, and compiler in order after
  the documented Marion MCP prefix;
- all child standard descriptors are terminals;
- caller cwd/environment and initial geometry arrive before exec;
- startup and live output reach the transparent terminal;
- exact input and resize reach the probe;
- the detached supervisor, journal, node PID, event stream, and cast exist;
- probe exit 37 becomes Marion exit 37 after terminal restoration.

A separate test starts a managed pane, waits causally until pre-attach output is durable,
then opens ordinary attach and requires the preexisting screen to reconstruct without
new process output. This kills the current live-only subscription bug.

A separate real-Claude compatibility cell uses the accepted pinned Claude binary and
the canned provider, reaches the real trust/composer UI, exercises normal settings/model
flags, detaches without a paid turn, and fails loudly when the pinned binary is absent.

Runner-level deadlines diagnose deadlocks. They never establish success or concurrency.

## Mutation requirements

Before completion, independently apply and restore these mutations:

| Mutation | Required failing evidence |
| --- | --- |
| Route `claude` through the legacy parser | probe never starts / usage assertion |
| Keep global help interception | help-containing argv test |
| Consume, reorder, UTF-8 decode, or drop an argument | indexed raw argument equality |
| Omit cwd/environment/initial size | probe launch-context assertions |
| Restore managed Claude flags | exact compiler prefix/tail assertion |
| Spawn Claude directly from the client | supervisor journal/PID/cast assertions |
| Use the grid view for the facade | raw OSC/query/output assertion |
| Subscribe without replay or activate before response | startup marker/sequence assertion |
| Drop exact byte encoding | non-UTF-8 input/output assertions |
| Resize only the local view | probe SIGWINCH/kernel-size assertion |
| Swallow the child result | expected process exit 37 |
| Break real Claude flag compatibility | pinned real-Claude cell |

## Targeted complexity cleanup

Only two behavior-neutral dependency moves accompany this slice:

1. Move `Auth` from the adapter hub into a leaf `auth` module so harness implementations
   do not import the module that imports them.
2. Move `ROOT_DEPTH` from `root` into a neutral lifecycle/depth leaf so `run` does not
   reach back into `root` solely for one constant.

No supervisor-wide repository, protocol, registry, or TUI reorganization is part of
this slice. Sentrux's pre-change quality signal is 5146, with acyclicity the bottleneck.
`session_end` is used after the change as a structural non-regression signal, not as an
acceptance oracle.

## Implementation order

1. Add opaque OS values and backward-compatible native launch fields with serialization
   and refusal tests.
2. Add the direct command dispatch and native Claude compiler.
3. Carry cwd/environment/geometry through root launch and apply geometry before exec.
4. Add exact PTY byte negotiation and compatible cast extensions.
5. Add atomic replay/live subscription.
6. Add the transparent terminal guard/view and foreground lifecycle result.
7. Add deterministic E2E and real-Claude compatibility coverage.
8. Apply the two dependency-cycle extractions.
9. Run targeted tests, affected legacy targets, workspace verification, mutations, and
   the Sentrux session comparison.

Each step must compile and keep its affected tests green before the next step. Changes
should land as small logical commits rather than one cross-cutting commit.

## Non-goals

- Replacing the existing `marion run` grammar or semantics.
- Making every late/general attach a transparent raw replay.
- Client-owned roots, terminal FD passing, or a new supervisor architecture.
- Completing unrelated M3/M5 or section 11 items in this slice.
- Parsing Claude's entire evolving CLI or settings schema.
- Claiming byte-exact behavior on a platform whose opaque OS-value encoding is not yet
  implemented and tested.

## Acceptance criteria

This slice is complete only when all of the following are true:

- `marion claude <opaque argv>` invokes the accepted Claude binary with the documented
  Marion MCP prefix and otherwise exact user argument boundaries.
- Native settings, tools, hooks, permissions, resume/model behavior, and TUI protocols
  are not suppressed by Marion-managed flags or grid rendering.
- caller cwd/environment and initial terminal size, input, output, resize, detach, and
  exit status cross the real CLI-supervisor-PTY path.
- startup output cannot be lost or duplicated at the replay/live splice.
- existing `marion run`, managed pane, general attach, protocol fixtures, and milestone
  tests remain compatible.
- every named deterministic and real-Claude test passes, every listed mutation is killed
  for the intended reason, fmt/clippy/workspace tests pass, and Sentrux reports no
  structural regression.
