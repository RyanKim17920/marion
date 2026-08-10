# Native Facade Runtime Design

**Status:** Approved design continuation
**Date:** 2026-08-10
**Scope:** Direct interactive native facades, per-lane vendor enablement, and a byte-transparent PTY
runtime. This design extends the production-dark native-facade foundation at `27fe62f`; it does
not itself enable a production descriptor.

## 1. Outcome

Marion will offer a vendor-shaped command such as `marion claude <opaque vendor flags>` only as an
explicit, direct, interactive root session. When the caller owns a foreground controlling TTY,
Marion starts the vendor's native terminal application under a supervisor-owned PTY and relays its
bytes without screen scraping or re-rendering. Output produced before the client is ready is
replayed from a durable binary stream and then followed through the same cursor, so the transition
cannot lose or duplicate bytes.

Structured operation remains a separate lane. `marion run`, MCP requests, automation, redirected
commands, and every subagent launch use a typed structured or ACP control plane. They never acquire
a native pane merely because a TTY happens to be present and never expose a vendor's subagent UI.

The same registry names both lanes, but selection is driven by the explicit request shape and
caller provenance. TTY detection proves that a native request can run; it never chooses native
mode.

## 2. Requirements and invariants

### 2.1 Functional requirements

1. A direct CLI invocation of a ready facade selector preserves the vendor argument tail as opaque
   `OsString` values and, when eligible, opens the vendor's native interactive UI.
2. Native output and input are byte-exact, including NUL, C1 controls, invalid UTF-8, split UTF-8,
   escape sequences, and large paste bursts.
3. A client joining after process start sees the complete stream from its initial terminal state,
   followed atomically by live traffic.
4. Exactly one connection owns keyboard input and PTY resize authority. Other attachments are
   explicitly read-only.
5. Client detach or failure restores the local terminal and releases its lease without killing the
   vendor process. Reconnection rebuilds the terminal from the durable stream.
6. Vendor-specific injection, behavioral readiness, executable identity, version policy, and
   conflicts are owned by that vendor's registry adapter.
7. `doctor` can explain installed, compatible, injection-ready, behavior-ready, and lane-enabled as
   distinct facts.

### 2.2 Hard invariants

- Native mode requires both an explicit native facade request and a `ControllingTtyWitness`.
- Structured, MCP, automation, and subagent requests cannot construct that witness-bearing native
  launch type.
- Only the native fd-bearing bootstrap can mint the same-connection, single-use
  `DirectCliCapability`; a launch ticket cannot substitute for it.
- Each lane is enabled independently and is bindable only when current computed readiness is
  `Ready`; native roots retain the canonical native `AgentType`.
- The supervisor remains the only owner that reads from and writes to the PTY master. A client is a
  listener and optional lease holder, never a PTY owner.
- The authoritative PTY stream is binary and sequenced. `pty.cast` is a derived compatibility
  artifact, not the replay cursor.
- Input travels byte-exactly to the PTY, but default evidence never persists or replays its payload.
- A PTY end record is committed after the reader drains final output. Journal node state does not
  terminate a pane relay.
- No vendor lane is production-ready merely because it is registered, enabled, installed, or
  advertises a protocol. Binding requires computed readiness for one concrete executable identity.
- Marion never installs or upgrades a vendor executable implicitly.

## 3. Explicit mode contract

### 3.1 Request types

The dispatcher lowers command provenance into a closed enum before vendor binding:

```text
LaunchIntent
  = NativeFacade {
      selector,
      opaque_tail,
      origin: DirectCli,
      capability: DirectCliCapability,
    }
  | StructuredCli {
      harness,
      request,
    }
  | StructuredRemote {
      protocol: Mcp | Acp,
      authenticated_peer,
      request,
    }
  | StructuredSubagent {
      parent,
      contract,
      depth,
    }
```

`NativeFacade` has no variant for MCP, ACP, a child node, an automation, or an implicit default.
Its constructor is private to the native bootstrap handler and requires a consumed
`DirectCliCapability`; ordinary root handlers cannot manufacture the enum by filling public fields.
The native process constructor accepts only that variant. This is stronger than checking booleans
near spawn because invalid combinations are unrepresentable.

### 3.2 Native CLI eligibility

Selector-first syntax is the explicit request: `marion <registered-facade> <opaque-tail...>`.
Unknown selectors continue to the existing parser exactly as the foundation specifies. A matched
selector is eligible for native mode only when all of these are true:

- the origin is the direct root CLI process;
- stdin is a terminal and is Marion's controlling terminal;
- Marion's process group is the terminal's foreground process group;
- stdout is the same verified controlling terminal;
- that descriptor's native lane is independently enabled and its computed readiness is `Ready`.

The check returns a move-only `ControllingTtyWitness` containing the opened descriptors, original
termios, original file-status flags, terminal identity, and initial geometry. Later layers do not
repeat `isatty` guesses.

This requirement explicitly supersedes the older non-TTY proxy clause: a process whose stdin or
stdout is redirected cannot open a separate `/dev/tty` and proxy a native session through it. That
process must use the structured lane. There is one terminal identity from eligibility through
restore, which prevents output capture and signal ownership from silently changing underneath the
session.

The witness never crosses the socket as a claimed boolean. The direct CLI opens the native-only
local bootstrap connection and sends duplicated stdin/stdout descriptors with `SCM_RIGHTS`, plus
the selector and a hash of the exact native request context. The supervisor authenticates the
local peer, independently checks that both descriptors name the same controlling terminal, checks
the peer process group against `tcgetpgrp`, reads initial geometry with `TIOCGWINSZ`, and compares
the selector/context hash with the request envelope. Linux uses `SO_PEERCRED` and macOS uses the
corresponding local peer-pid credential; platforms without peer credentials plus descriptor
passing report native mode unsupported.

`DirectNativeRequestContext` is distinct from the later `NativeNodeContext`: it contains canonical
project identity, selector, opaque argument tail, terminal profile, and native wire version, but no
not-yet-minted agent id and no secret. Its hash is BLAKE3 with a fixed domain tag over
length-prefixed raw Unix `OsStr` bytes and fixed-width semantic fields; argument boundaries and
empty arguments are therefore unambiguous. Both peers compute it from the same authenticated
envelope. Secrets and environment values are never hash inputs.

Only that `ConnectionKind::NativeBootstrap` handler can issue a `DirectCliCapability`. The
capability is a random 256-bit opaque id stored server-side and bound to the authenticated
connection id, peer pid/uid, terminal fingerprint, selector, context hash, initial geometry, and a
monotonic ten-second expiry. It is single-use. The native launch request must arrive on the same
connection with the exact selector/context hash; validation and consumption occur atomically before
descriptor binding, executable detail, readiness detail, injection, filesystem side effects, or a
launch ticket. A mismatch, replay, expired id, swapped descriptor, or different connection is an
authorization refusal with no side effect.

The ordinary root RPC enum, MCP/ACP transports, automation entry points, and subagent constructors
contain neither the native bootstrap method nor a capability field. Presenting copied capability
bytes on one of those connection kinds is rejected before lookup. This is an unforgeable Marion
state-machine capability, not a claim that same-account hostile code cannot inspect its own
terminal.

A matched native facade without a controlling TTY is refused with a stable message directing the
operator to a structured command. Marion does not silently reinterpret opaque native arguments as
a prompt and does not fall back to a different vendor surface.

### 3.3 Explicit noninteractive behavior

The following paths are always structured, even when launched from a terminal:

- `marion run ...` and other existing structured CLI verbs;
- supervisor RPC and MCP entry points;
- ACP sessions selected by a concrete ACP agent descriptor;
- scheduled, piped, redirected, or background execution;
- every child or subagent launch, regardless of parent mode;
- recovery/restart of a structured node.

These paths use their declared typed stream or ACP/MCP session. They do not allocate a display PTY,
publish a pane, reserve a keyboard lease, or reveal a vendor's native subagent catalogue. If a
vendor has no compatible structured lane, the request is refused as unsupported; it is never
upgraded to native mode.

## 4. Dual-lane registry

### 4.1 Descriptor model

One facade selector resolves to a vendor identity and two independently gated lanes:

```text
FacadeDescriptor {
  identity: VendorIdentity,
  primary_selector,
  aliases,
  native: Option<Lane<NativeLane>>,
  structured: Option<Lane<StructuredLane>>,
}

Lane<T> {
  enabled: bool,
  config: T,
}

NativeLane {
  root_agent_type: AgentType,
  executable: ExecutablePolicy,
  versions: VersionPolicy,
  injection: NativeInjectionAdapter,
  readiness: NativeReadinessProbe,
  stream_version: PtyStreamVersion,
}

StructuredLane {
  agent_identity: StructuredAgentIdentity,
  control: Typed | Acp | Mcp,
  versions: VersionPolicy,
  adapter: StructuredAdapter,
  readiness: StructuredReadinessProbe,
}
```

`VendorIdentity` is stable Marion data, not whatever basename happened to resolve on `PATH`.
`StructuredAgentIdentity` names a concrete agent implementation and protocol version; `ACP` alone
is a protocol capability, not an agent identity. `NativeLane::root_agent_type` is the existing
canonical native root `AgentType`; selector and vendor branding never replace it in journal,
registry, restart, or root-depth semantics.

### 4.2 Resolution and collisions

The existing facade resolver remains the single owner of selector matching, primary names, aliases,
and registration-order-independent collision diagnostics. Binding proceeds in this order:

1. resolve the selector to one descriptor;
2. select a lane from `LaunchIntent`, never from availability;
3. resolve and authenticate the concrete executable;
4. enforce version policy;
5. calculate injection and reject conflicts;
6. compute the selected lane's readiness from current evidence;
7. require that lane to be enabled and computed `Ready`;
8. assemble the process without re-resolving any prior decision.

Native and structured enablement and readiness are independent. A vendor may be enabled and ready
in one lane and disabled or blocked in the other. `doctor` reports two rows rather than collapsing
that state into a misleading vendor boolean.

### 4.3 Per-lane enablement and computed readiness

```text
LaneReadiness {
  enabled,
  installed,
  version_compatible,
  injection_ready,
  behavior_ready,
  result: Disabled | Blocked(reason) | Ready,
}
```

Enablement is descriptor policy; readiness is a value computed at `doctor` or bind time from the
current executable, version, injection, and behavioral evidence. It is not a stored state machine
that can remain ready after its evidence changes. Successful binding requires
`lane.enabled && readiness.result == Ready`. Tests may construct enabled synthetic lanes, but
production lanes remain absent or disabled until their rollout milestone is accepted. A readiness
failure blocks only the selected lane and does not mutate or disable the other lane.

## 5. Vendor rollout

The rollout order is evidence-driven and does not encode a permanent vendor hierarchy.

### Wave 0: contract remains production-dark

Land the mode types, dual-lane schema, doctor rows, injection result, terminal witness, and PTY
stream behind synthetic descriptors. The production ready slice remains empty. This proves that
legacy CLI, structured launch, MCP, and existing attachment behavior cannot accidentally enter the
new runtime.

### Wave 1: first terminal-native proving adapter

Claude Code and Codex CLI are candidates because the repository already has terminal capture and
PTY behavior evidence for their pane surfaces. Candidate status is not enablement. Choose the
first adapter only after re-running the installed-executable, version, injection, first-output,
resize, interrupt, exit, and cleanup probes against its concrete first-party identity. Activate
one selector and keep the second production-dark until it passes the identical matrix.

### Wave 2: second independent vendor

Activate a second vendor whose injection and readiness mechanism differs from Wave 1. This proves
that the registry boundary is real rather than one vendor implementation hidden behind generic
names. Gemini CLI must be identified and probed as Gemini CLI. Antigravity is a distinct product
identity and must not inherit Gemini's selector, executable, credentials, version claims, or
readiness evidence without first-party proof.

### Wave 3: structured protocol expansion

Add concrete ACP-capable agents one descriptor at a time. Each row names the agent, executable,
ACP version/capabilities, initialization identity, and behavioral probe. A generic `acp` selector
is not activated as if it were an agent. Community vendors enter through the same readiness and
security gates as first-party candidates.

At every wave, aliases are added only after the primary selector's lane is enabled and collision
tests pass. A
vendor can be withdrawn by data without changing dispatch, PTY, or structured runtime code.

## 6. Injection, readiness, and conflicts

### 6.1 Pure injection plan

Structural opacity from the foundation remains intact. `NativeInjectionAdapter` receives only the
semantic `NativeNodeContext`; it cannot inspect the resolved executable, vendor program path, or
opaque user tail:

```text
NativeNodeContext {
  agent_id,
  canonical_root_agent_type: AgentType,
  project_scope,
  session_id,
  auth_mode,
  credentials: CredentialRefs,
  permission_policy,
  terminal_profile,
}

NativeInjection {
  prefix: Vec<OsString>,
  env: SecretAwareEnv,
  documents: Vec<PrivateArtifact>,
}
```

`prefix` is Marion-owned vendor syntax placed before the user tail. `env` is a set/remove plan with
explicit secrecy and ownership metadata. Each document contains a validated relative name, bytes,
and process lifetime; it never contains an absolute host path supplied by the adapter.

The generic assembler alone receives both `BoundProcess { program, opaque_tail }` and the adapter
result. Its one ordering rule is `program + injection.prefix + opaque_tail`; no adapter callback is
allowed after the tail enters this boundary. It materializes documents, applies environment changes
without shell interpolation, and returns the complete command. The adapter therefore cannot drop,
rewrite, reorder, parse, or condition injection on user arguments, and the assembler cannot invent
vendor-specific syntax.

The generic artifact executor runs cleanup on refusal, launch failure, panic, and process exit.
Client detach cleans only client-owned state; documents still required by a surviving vendor process
remain until that process ends. The launcher never edits a user's global vendor configuration.

### 6.2 Conflict classes

Every injected value and reserved flag spelling is declared as descriptor data, outside the opaque
adapter. The generic assembler, which alone sees the tail, applies one conflict policy:

- **Reserved:** Marion must own the value for protocol identity or security. An explicit user
  value is a refusal with both owners named.
- **Defaultable:** Marion supplies it only when absent. A user value remains byte-for-byte intact.
- **Composable:** the adapter defines a deterministic merge order and an assertion proving the
  vendor received both values.

Unknown overlap is a refusal, not last-writer-wins. Secret values are named by key in diagnostics
but never rendered. Conflict evaluation returns ownership facts to the assembler; it never passes
the tail back to the adapter. The descriptor owns vendor spellings once, and the launcher contains
no vendor branch.

### 6.3 Readiness

Static readiness checks executable identity, version, required files, platform support, and
conflicts without spawning. Behavioral readiness runs a bounded vendor-specific micro-session in a
private directory and proves:

- process start and expected identity/version;
- injection consumption rather than mere environment presence;
- first meaningful output or protocol initialization;
- interrupt and bounded clean termination;
- no leaked child, socket, document, credential, or terminal state.

Readiness uses explicit barriers and protocol observations. Elapsed time is diagnostic only; a
fixed latency threshold never decides correctness. The generic probe runner binds the executable
and opaque probe tail, asks the adapter only for a synthetic `NativeNodeContext` injection, and uses
the same generic assembler as production.

Cache entries are keyed by resolved executable identity, file identity/mtime, version, adapter
revision, platform, and a secret-free configuration fingerprint. That fingerprint contains only
key names, presence/absence, provenance class, non-secret file identity/mtime, and public policy;
it contains neither a secret value nor a raw or reversible digest of one. Behavioral probes use a
disposable canned/non-secret credential rather than an operator credential. Any key change
invalidates the result.

`doctor` shows separate columns for installed, version, injection, behavioral probe, lane enabled,
and the exact reason for the first failure.

## 7. Native process and pane data flow

```text
Direct CLI
  -> facade resolver
  -> ControllingTtyWitness
  -> native-only fd-bearing bootstrap handshake
  -> same-connection DirectCliCapability
  -> authenticate + atomically validate/consume capability
  -> select enabled native lane / compute readiness
  -> mint agent id + NativeNodeContext / opaque injection / generic assembly
  -> durable SpawnIntent + artifacts + pre-sized PTY/stream
  -> exec-gate helper spawn + durable Started record + vendor-exec release
  -> PaneOwner publishes live host + returns launch ticket
  -> client opens pane, enters transparent relay, sends initial size
  -> one durable cursor replays and follows
  -> reader drain + PTY End
  -> local terminal restore + command result
```

Authorization and reservation are deliberately different capabilities. `DirectCliCapability`
authorizes one native bind/launch and is consumed before binding or side effects. After descriptor,
executable, version, and readiness checks succeed, the supervisor mints the agent id and
`NativeNodeContext`, obtains opaque injection, performs generic assembly, durably appends
`SpawnIntent`, prepares the cleanup manifest/artifacts, and creates a server-side launch-ticket
reservation bound to
project, authenticated connection, descriptor identity, context hash, agent id, stream session,
initial geometry, and expiry. The ticket authorizes only later pane-open/writer priority; it cannot
authorize a process launch. It is not returned until `Started` is durable and the pane exists, and
it is destroyed on any preceding failure.

The initial geometry comes from the server-verified bootstrap descriptors. Before `exec`, the
supervisor opens the PTY master, applies that geometry with `TIOCSWINSZ`, reads the effective value
back with `TIOCGWINSZ`, and writes that read-back value into the durable stream header. Any failure
refuses the launch before a child exists. `PtyHost` starts the stream and reader only after this
pinning and before spawn. A geometry change between bootstrap and `pane-ready` becomes the first
ordered live `Resize`; it never changes what the header truthfully says the child saw at exec.

Publishing still waits until the process exists, but bytes produced between spawn and publication
are already in the durable stream. The launch ticket reserves the initiating connection's writer
priority without advertising a nonexistent child. `PaneOwner` remains a narrow publish/unpublish
seam; `root.rs` does not learn about sockets or clients.

## 8. Durable binary sequenced stream

### 8.1 Authoritative file

Each native pane owns `pty.stream` beside `pty.cast`:

```text
StreamHeader {
  magic,
  version,
  initial_cols,
  initial_rows,
  terminal_profile,
  session_id,
  header_checksum,
}

StreamRecord {
  encoded_len,
  record_seq,
  display_seq: Option<u64>,
  mono_ns,
  kind,
  payload,
  checksum,
  commit_trailer,
}

kind = Output(bytes)
     | InputEvidence(input_seq, byte_len, keyed_digest, after_display_seq)
     | Resize(cols, rows)
     | End(journal_event_id, exit_code, signal, timed_out, disposition)
     | Fault(category, message)
```

`record_seq` is dense across every private evidence record. `display_seq` is present and dense only
for output, resize, fault, and end, so geometry and termination have an exact public order relative
to displayed bytes. By default Marion never persists an input payload. Before writing input to the
master it commits only `InputEvidence`: a dense private input ordinal, byte length, a keyed BLAKE3
digest, and the current display-order boundary. The random digest key lives only in locked process
memory and is never written, logged, cached, or sent, preventing offline guessing of short secrets.
There is no input-payload opt-in in the initial runtime.

Input evidence has no display sequence and is never emitted by pane replay; the cursor consumes it
without constructing a wire frame. This prevents a read-only attacher from learning typed secrets
while proving length, identity-within-this-live-session, and order. `pty.cast` omits input payload
records and is projected only from supported output/resize/end records for asciicast compatibility.
It may not represent arbitrary invalid UTF-8 and therefore cannot be the authoritative binary
record.

All sizes are checked before allocation. The header is at most 4 KiB; an encoded record is at most
1 MiB; PTY output/input chunks are split at 64 KiB; and a base64 JSON pane frame is therefore at
most 96 KiB including metadata. The default committed stream limit is 1 GiB per session. Reaching
any hard bound follows the attributed stream-failure path rather than allocating, truncating a
logical packet, or silently stopping recording.

### 8.2 Commit protocol

`PtyStreamWriter` serializes these fields under one lock:

```text
next_record_seq
next_display_seq
committed_offset
generation
state: Open | Ended | Faulted
```

The header is encoded, `write_all`-written, flushed, and `sync_data`-committed before a child may be
spawned. Every record then follows this exact order while the sequencer lock excludes another
writer:

1. Check stream/record bounds and reserve, but do not publish or advance, `record_seq` and optional
   `display_seq`.
2. Encode the fixed header, payload, BLAKE3 checksum over header plus payload, and a fixed commit
   trailer repeating magic, encoded length, record sequence, and checksum.
3. `write_all` the entire encoded record at the current committed offset.
4. Call `flush`, then `sync_data`. Either failure leaves the in-memory committed offset and both
   next-sequence counters unchanged and enters terminal stream-failure handling.
5. Only after successful `sync_data`, advance `committed_offset` and sequence counters, update
   `Open/Ended/Faulted`, increment `generation`, and release the lock.
6. Wake cursor pumps. They read the now-durable record and enqueue any display frame into bounded
   per-connection FIFOs; socket I/O never occurs under the sequencer lock. Thus publication cannot
   precede durable commit.

The derived `pty.cast` projection runs only after the raw commit and is not in this transaction. A
projection failure marks the cast incomplete without invalidating the authoritative stream or
reordering pane publication.

After restart, recovery validates the header checksum and scans from its exact end. A record is
committed only when its declared length is within bounds, its complete payload exists, its checksum
matches, its commit trailer matches magic/length/sequence/checksum, and both record/display
sequences are the expected dense values. Recovery may truncate only a short final record or partial
final trailer at physical EOF, and it calls `sync_data` after `set_len(last_valid_offset)`. A complete
record with a bad checksum/trailer/sequence, any invalid record followed by more bytes, an invalid
header, or bytes after a valid `End` is interior corruption: quarantine the stream and refuse replay;
never scan ahead or truncate it into apparent validity.

An authoritative stream-write failure is terminal for the pane. The supervisor records an
attributed internal failure, refuses more input/attachments, kills and reaps the child through the
normal owned-process path, and emits `Fault`/`End` where possible. It never claims a complete
replay after silently switching to volatile-only delivery.

### 8.3 Spawn, journal, and stream reconciliation

Normal launch uses a trusted exec-gate helper, not a blocking `Command::pre_exec` closure (which
would prevent `spawn` from returning on platforms whose parent waits for exec completion). The
generic assembler launches Marion's hidden gate mode as the first executable under the already
configured PTY. That helper inherits a release pipe and the already assembled vendor program/argv,
emits no output, and blocks before `execve` of the vendor. After `spawn` returns, the parent reads
the helper pid/start identity, appends and syncs journal `Started`, transitions and syncs the
cleanup manifest to `InUse`, and only then writes the release byte. The helper uses the same pid
across vendor `exec`. If the parent fails or dies before release, pipe EOF makes the helper `_exit`
without executing the vendor. This closes the otherwise unaccountable window in which vendor
output/process state could exist without durable `Started` identity.

Normal terminal ordering is equally strict: observe/reap the process, drain the PTY reader to EOF,
append and sync the journal terminal event, append and sync stream `End` referencing that journal
event id, then publish `End`. The journal may therefore become terminal before the pane, but the
pane never becomes terminal first and clients wait for stream `End`.

Restart reconciles the two durable artifacts and exact pid/start-id audit as follows:

| Durable journal | Valid stream | Exact process audit | Required recovery |
|---|---|---|---|
| No `SpawnIntent` | Any orphan file | No attributable identity | Quarantine/remove only through the owned artifact manifest; never expose or signal a process. |
| `SpawnIntent`, no `Started` | Missing stream or header with no output/`End` | Exec gate is gone; vendor could not exec | Sync `SpawnAborted`; append/sync `End(NeverStarted)` only when the header is valid, otherwise record pane-unavailable and clean the owned artifact. |
| `SpawnIntent`, no `Started` | Any output record | Impossible under the gate | Hard `UnaccountableStart` incident; quarantine stream and do not fabricate clean `End`. |
| `Started`, no terminal event | No `End` | Matching process live | The PTY master was lost with the supervisor; attribute `SupervisorLost`, signal only the verified pid/group, confirm it is gone, sync the journal terminal event, then append/sync matching `End`. |
| `Started`, no terminal event | No `End` | Matching process gone | Sync terminal `SupervisorLost` with unknown exit status, then append/sync matching `End`; never invent an exit code. |
| Terminal journal event | No `End` | Process gone | Validate/truncate only an incomplete EOF tail, then append/sync `End` referencing the existing event. |
| Terminal journal event | Matching `End` | Process gone | Complete; replay is authoritative. |
| Missing/mismatched terminal event | `End` present | Any | Hard cross-artifact corruption; an `End` cannot synthesize or rewrite its required journal event. |
| `Started` or terminal event | Missing/corrupt stream | Process live or gone | Journal remains lifecycle authority, pane replay is unavailable, and a live exact-identity process follows the `SupervisorLost` termination path; never create a replacement stream that implies missing output. |
| Any terminal state | Any stream state | Matching process live | Hard identity conflict; do not overwrite either artifact or signal until the process-identity audit path resolves it. |

Stream recovery completes before this table is applied. A quarantined stream never becomes
attachable even if the journal can independently recover node state.

### 8.4 Atomic replay-to-live cursor

Pane setup is an explicit two-step handshake:

```text
node/pane-open(agent_id, wire = binary-v1)
  -> stream_id, writable, held_by, recorded_size

client enters relay and sends its initial size

node/pane-ready(stream_id, from_seq = 0)
  -> observed_cut_seq

node/pty-frame(stream_id, seq, kind, payload_base64)
...
node/pty-end(stream_id, seq, status)
```

`pane-open` pins either the live host or completed recording. No PTY frames are sent before
`pane-ready`, so the client never discards early replay while waiting for a response or enters its
terminal too late.

The server creates one `PtyStreamReader` at sequence zero. It reads the committed prefix, waits at
the same byte offset, and continues when the writer generation changes. `observed_cut_seq` is only
a test/diagnostic boundary describing what was committed at readiness; it is not a second cursor.
Because replay and follow use the same offset, no subscription window exists in which a packet can
be lost or duplicated.

The initial implementation carries bytes through a base64 `WireBytes` newtype in the existing
newline-framed JSON connection. Legacy UTF-8 `node/pty` remains legacy; the server does not send
binary frames to a client that did not negotiate `binary-v1`. A dedicated authenticated Unix pane
socket is a later transport optimization behind the same cursor if measurements justify it. The
PTY master itself is never transferred or duplicated between readers.

## 9. Transparent foreground relay

### 9.1 Terminal ownership

Create `RelayScreen` by reusing the current guard's termios snapshot, original file-status flags,
panic-hook chaining, sink serialization, and idempotent restoration. Drop restores the exact prior
panic hook rather than leaving one inactive hook per repeated session. Unlike the grid screen, it
does not emit Marion's alternate-screen preamble and does not feed output through `View`. It writes
decoded `Output` payloads byte-for-byte to the witnessed terminal.

A passive terminal-mode observer reuses Marion's terminal parser only to remember child-selected
alternate-screen, mouse, bracketed-paste, cursor, and related sticky modes for abnormal cleanup.
It never changes forwarded bytes. Normal child exit is allowed to restore its own modes; detach,
transport failure, panic, and external termination run the observer-derived cleanup followed by
termios and file-status restoration.

### 9.2 Writer lease and read-only clients

The existing one-connection `WriteLease` remains the authority for both input and remote resize.
The launch ticket gives the initiating direct CLI first claim. Later clients may attach read-only
and receive the same byte stream, but they cannot send input or alter the single PTY geometry.

All input bytes except Marion's explicit detach-prefix sequence are transported unchanged. A
rejected, expired, or foreign lease produces one session-level refusal; input notifications do not
produce per-keystroke error floods. Disconnect drops the lease automatically. The supervisor's
host reference and master remain alive.

### 9.3 Resize and signals

The relay marks geometry dirty initially, sends the witnessed size before `pane-ready`, and sends
later changes only from the writer. The supervisor reuses reader-thread resize ordering:
old-geometry output is drained, `TIOCSWINSZ` is applied and sequenced as `Resize`, then `SIGWINCH`
is sent to the child process group. Read-only clients resize only their local containing terminal;
they do not reflow the shared child.

Signal handlers perform only async-signal-safe notification through an atomic/self-pipe. In raw
mode, typed Ctrl-C, Ctrl-Z, and backslash sequences remain bytes for the child's PTY line discipline;
Marion does not translate them into parent-side signals.

- External `SIGWINCH`: mark dirty and send the next observed size.
- External `SIGTSTP`: restore the local terminal, install/default and re-raise stop; on `SIGCONT`,
  re-enter relay mode and force a resize.
- External `SIGINT`, `SIGTERM`, or `SIGHUP` directed at Marion: detach, restore, and release the
  lease. Do not infer permission to kill the supervised node.
- `SIGKILL`: cannot be handled; the next shell/terminal owner may need its normal reset behavior.

### 9.4 Exit, detach, and reconnect

The supervisor reaps or observes the child, drains the PTY reader to EOF, commits every final
`Output`, then commits `End`. The relay ignores a journal `NodeState` as an end condition and exits
only after consuming ordered `End`. A foreground `marion <facade>` maps that status to its command
result after restoration; a late `marion attach` reports the recorded status without becoming the
process owner.

Detach and socket loss do not add `End`, close the master, signal the child, or change node state.
A reconnect opens a new cursor at zero by default, recreates terminal state, then follows. A future
resume optimization may accept a client cursor only after validating session id, sequence, and
record checksum; an unverified cursor is never trusted.

## 10. Error behavior

- No controlling TTY: refuse native facade and name the explicit structured alternative.
- Lane absent, disabled, or not currently ready: report the requested lane and first failed
  readiness fact; do not choose the other lane.
- Executable missing, identity mismatch, or unsupported version: no injection files and no spawn.
- Injection conflict: name key/flag owners, redact values, and leave user configuration untouched.
- Readiness failure: report the causal probe observation, clean artifacts/processes, and keep the
  lane inactive.
- PTY stream corruption: refuse replay at the first invalid sequence/record; never skip forward.
- Stream write failure: attributed terminal failure and owned child teardown, never silent loss.
- Client backpressure: each connection FIFO is capped at 256 frames and 8 MiB of decoded payload,
  whichever comes first. An enqueue that would exceed either cap atomically detaches that client
  and releases its writer lease; it never drops an interior frame or blocks the durable writer.
  The child continues and reconnect recovers from the file.
- Terminal write failure: restore what remains restorable, detach, release lease, and preserve the
  supervised node.
- Cleanup failure: retain the primary outcome and append a named cleanup diagnostic without
  printing secrets.

## 11. Security, installation, and version policy

### 11.1 Authentication and capabilities

Authenticate the root peer before returning executable, injection, readiness, stream-path, or
process detail. Pane tickets and stream ids are unguessable capabilities bound to project,
connection, agent, negotiated wire version, and expiry. They cannot be replayed by another
connection or project.

Native terminal output is intentionally unsanitized because transparency is the product contract
and terminal control sequences are the vendor UI. It is delivered only for the explicitly selected,
authenticated node. Structured/TUI surfaces continue to render structured data and do not inherit
this trust model.

### 11.2 Executable identity and installation

Resolve executables without a shell, preserve opaque argument bytes, canonicalize symlinks where
the platform supports it, and record file identity with the measured version. Descriptor policy
may constrain acceptable publishers/paths, but PATH basename alone never proves vendor identity.

Marion does not download, install, log in, upgrade, or edit global configuration for a vendor. A
missing or incompatible binary yields a doctor instruction and launch refusal. Installation remains
an explicit operator action through the vendor's supported channel.

The artifact executor opens the agent directory once as a directory fd with
`O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW`, then `fstat`s it. Its owner must be the authenticated uid and
its group/other permission bits must be zero; otherwise launch is refused rather than chmodding an
unexpected object. `PrivateArtifact` names are relative and bounded to 4 KiB, with at most 16
documents, 1 MiB each, and 8 MiB total. Validation rejects absolute paths, empty components, `.`,
`..`, NUL, and overlong components.

All traversal is dirfd-relative. On platforms with `openat2`, use `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS`;
otherwise walk components using `mkdirat`/`openat` with `O_NOFOLLOW` and verify each opened fd.
Temporary files use unpredictable names and `O_CREAT|O_EXCL|O_NOFOLLOW` with mode `0600`; after
write they are `fsync`ed and verified as regular/current-uid/mode-`0600`/link-count-one. Destination
publication never replaces an existing name: use dirfd-relative `renameat2(RENAME_NOREPLACE)` where
available, or the portable `linkat(temp, final)` operation (which fails on an existing destination)
followed by unlinking the temporary name only after the link succeeds. Reverify the final fd as
regular/current-uid/mode-`0600`/link-count-one and `fsync` the directory after publication. No
absolute cleanup path is ever retained.

`SecretAwareEnv` is a move-only plan of `Public(OsString)` or `Secret(SecretRef)`. A vendor adapter
can select only an opaque `SecretRef` present in `NativeNodeContext`; it cannot resolve, format, or
inspect credential bytes. After conflict checking and immediately before child environment setup,
the generic materialization boundary alone resolves each reference into owned `SecretBytes`.
`SecretBytes` implements neither `Debug`, `Display`, serialization, nor cloning and zeroizes its
buffer on drop. Logging receives only `{key, set_or_remove, provenance, secret: bool}`; errors name
the key and owner but never value, length, hash, or prefix. Secrets do not appear in argv,
documents, cast, stream metadata, readiness fingerprints, error text, or debug logs.

Before creating the first document, the executor atomically writes and directory-syncs a cleanup
manifest whose lifecycle is `Planned -> Materialized -> InUse(pid,start_id) -> CleanupPending ->
Clean`. Every transition is temp-write, file-sync, dirfd-relative rename, and directory-sync.
Refusal before spawn moves directly to cleanup; launch marks `InUse` only after durable `Started`;
client detach leaves it `InUse`; normal exit marks `CleanupPending` after process death. Restart
audits the exact pid/start identity: retain artifacts while it matches live, otherwise unlink only
manifest-listed relative regular files through the verified dirfd, fsync the directory, and remove
the manifest last. A failed unlink remains `CleanupPending` with a redacted diagnostic for retry.

### 11.3 Version negotiation

Three versions remain distinct:

1. vendor executable version and identity;
2. structured protocol/capability version, when that lane uses ACP or MCP;
3. Marion's PTY stream and binary wire version.

Each is negotiated or gated independently. The descriptor declares supported vendor ranges and
probe revision. ACP initialization must return the same concrete agent identity the descriptor
selected. `binary-v1` is negotiated before frames; unknown record kinds are rejected unless that
stream version explicitly defines length-skippable extensions. Downgrade never converts arbitrary
bytes through UTF-8.

## 12. Deterministic verification

Tests use pipes, socket messages, PTY ioctls, and explicit hooks as causal barriers. Timeouts are
deadlock ceilings only; no correctness assertion depends on elapsed milliseconds.

### 12.1 Mode and registry matrix

- Selector-first direct CLI plus foreground controlling TTY constructs `NativeFacade`.
- The same selector under pipe, redirection, background process group, or no controlling terminal
  is refused without spawn or injection.
- A redirected process that opens `/dev/tty` is still refused; the removed non-TTY proxy behavior
  cannot satisfy native bootstrap.
- Swapped/non-terminal/different-terminal descriptors, peer-pid mismatch, selector/context-hash
  mismatch, expired/reused token, and token presentation on another or ordinary RPC connection all
  refuse before descriptor detail or side effects. Capability expiry tests use an injected clock.
- `marion run`, MCP, ACP, automation, and subagent launch stay structured even with a TTY.
- A native-only descriptor refuses structured use; a structured-only descriptor refuses native
  use; neither falls across lanes.
- Native and structured `enabled` bits and readiness reports cover all four independent
  combinations; a failure in one does not mutate the other, and every native journal row retains
  the canonical native `AgentType` rather than a selector-derived value.
- Alias/primary collision results are independent of registration order and name both owners.
- Production-dark descriptors cannot assemble or spawn.

### 12.2 Vendor and injection tests

- Each activated vendor passes executable identity, supported/unsupported version, injection
  consumption, reserved/defaultable/composable conflict, secret redaction, cleanup, interrupt, and
  no-child-leak cases.
- Compile-time/API tests prove `NativeInjectionAdapter` can receive only `NativeNodeContext`; the
  generic assembler alone receives `program` and `opaque_tail`, and mutations dropping/reordering
  `program + prefix + tail` fail byte-exact argv assertions.
- Artifact tests attack absolute/`..`/`.`/empty/NUL/overlong paths, ancestor and final symlinks,
  hard links, wrong owner/mode/type, rename replacement, document count/size totals, and cleanup
  manifest restart at every lifecycle state.
- `SecretAwareEnv` cannot be formatted or serialized, redacted diagnostics contain key/provenance
  only, buffers zeroize on drop, and readiness fingerprints remain identical across secret-value
  rotations while changing for public provenance/configuration changes.
- Behavioral readiness observes a child barrier/protocol frame rather than sleeping.
- Cache invalidates on executable file identity, version, adapter revision, configuration, and
  platform changes.
- Gemini and Antigravity fixtures cannot satisfy one another's identity or selector.
- ACP tests assert the concrete initialized agent identity, not merely protocol success.

### 12.3 PTY splice and relay tests

1. **Early/live splice:** child writes binary `A`, signals a control pipe, and blocks. Attach after
   the signal, release it, then child writes `B` and exits. Assert exact `A || B`, dense sequence,
   and one ordered `End`.
2. **Commit race:** pause after physical append but before committed-offset publication. Open the
   attach, release the hook, and assert the record appears exactly once.
3. **Binary corpus:** round-trip NUL, every byte value, invalid UTF-8, C1, ESC, split multibyte
   sequences, and a large paste. Quote the explicit detach prefix through the key filter and test
   unquoted detach separately. Compare byte count and digest in both directions.
   Inspect `pty.stream`, `pty.cast`, frames, diagnostics, and readiness artifacts to prove no input
   payload or reversible digest persists; assert only ordinal, length, keyed digest, and order
   evidence and prove pane replay emits no input frame.
4. **Resize:** child reports old `TIOCGWINSZ`, blocks, test resizes the outer PTY and signals Marion,
   then child reports again. Its first pre-exec-gate observation must equal the stream header's
   `TIOCGWINSZ` read-back; assert `old output < Resize < new output` afterward.
5. **Signals:** raw `0x03` reaches the child and does not terminate Marion; external stop/continue
   restores/re-enters and forces resize; external termination restores and detaches without killing
   the child.
6. **Exit drain:** child writes `LAST` and exits immediately. Assert `Output(LAST).seq < End.seq`,
   stdout contains `LAST`, and restore follows it.
7. **Crash/reconnect:** kill the relay client. Assert child/master survive, lease releases, and a
   new client receives the prefix and subsequent bytes once.
8. **Mode cleanup:** child enables alternate screen, mouse, bracketed paste, and hidden cursor, then
   crashes. Assert termios, file flags, and tracked modes return to baseline.
9. **Completed replay:** open a finished recording after live-host removal and assert the same
   output, resize history, and exit status.
10. **Read-only:** second client sees identical output but input and remote resize are refused
    without affecting the writer.
11. **Durability boundaries:** crash/fail independently after body write, flush, `sync_data`,
    in-memory committed-offset advance, wake, journal `Started`, journal terminal event, and stream
    `End`. Assert the exact truncation/publication rule and every reconciliation-table row.
12. **Bounds/backpressure:** exercise exact-minus-one/exact/plus-one header, record, document,
    stream, frame-count, and queue-byte limits. Overflow detaches only the slow client, releases its
    lease, preserves the child, and replay recovers every frame without a gap.

### 12.4 Required mutation checks

The verification lane must kill each of these mutations and record the causal assertion:

- infer native mode from TTY without explicit facade intent;
- restore the non-TTY `/dev/tty` proxy or mint/present a native capability through ordinary
  root/MCP/automation/subagent transport;
- omit terminal-fd/peer/connection/selector/context binding, allow capability reuse/expiry bypass,
  or let a launch ticket authorize launch;
- allow MCP or a subagent to construct a native launch;
- fall back from an unavailable structured lane to native;
- replace independent lane enablement/computed readiness with one vendor-global/stored activation,
  or derive the native root `AgentType` from the selector;
- activate on installed/version-only readiness;
- accept an executable basename without identity/version proof;
- let user injection silently override a reserved Marion value, or leak a secret in diagnostics;
- expose program/tail to `NativeInjectionAdapter`, return a complete argv from it, or assemble any
  order other than `program + prefix + opaque_tail`;
- accept traversal/symlink/wrong-owner/wrong-mode artifacts, perform path-based cleanup, skip a
  manifest sync/transition, persist a secret/reversible secret fingerprint, or format secret bytes;
- capture replay cut and register follow in separate cursors;
- open the PTY cursor at current EOF instead of sequence zero;
- advance committed offset, wake, or publish before complete write/flush/`sync_data`, accept a
  missing/mismatched commit trailer, truncate interior corruption, or accept a sequence gap;
- violate any spawn-gate/journal/`End` reconciliation row, release exec before durable `Started`,
  publish `End` before its referenced terminal journal event, or invent a recovered exit code;
- convert input or output through UTF-8;
- persist/replay an input payload instead of ordinal/length/keyed-digest/order evidence;
- apply initial geometry after exec, omit `TIOCGWINSZ` read-back/header equality, omit initial resize,
  or let a read-only client resize the child;
- remove record/document/stream/queue bounds, drop an interior frame on overflow, or let a slow
  client block the sequencer;
- allow another client to beat the launch-ticket writer reservation;
- terminate relay on journal `NodeState` or emit `End` before reader drain;
- close the master or kill the node on client disconnect;
- feed transparent output through `View` or emit Marion's alternate-screen preamble;
- omit normal, panic, external-signal, or stop/continue terminal restoration;
- treat Gemini and Antigravity as the same descriptor identity;
- accept ACP initialization from a different concrete agent identity.

## 13. Milestones and gates

### Milestone 0 — explicit contract, production dark

Add `LaunchIntent`, fd-bound `DirectCliCapability`, `ControllingTtyWitness`, dual-lane descriptor
types, independent enablement/computed-readiness reports, canonical native root `AgentType`, and
doctor rows with synthetic-only tests. Gate: legacy/structured/MCP/subagent matrices prove zero
native construction; production native lanes remain absent or disabled.

### Milestone 1 — injection and readiness kernel

Add opaque `NativeNodeContext -> prefix/env/documents` injection, generic assembly, conflict
policies, executable/version identity, behavioral probe runner, secret-free cache keys, dirfd-only
private artifact executor, and durable cleanup lifecycle. Gate: synthetic adapter matrix and
security mutations; production lanes remain disabled.

### Milestone 2 — durable PTY stream

Add `pty.stream`, writer/recovery/cursor, binary wire negotiation, pane-open/ready, ordered end, and
completed replay. Gate: splice, commit-race, binary, corruption, final-byte, and reconnect tests.

### Milestone 3 — transparent direct CLI relay

Add launch-ticket writer priority, `RelayScreen`, byte-exact input/output, resize/self-pipe signal
loop, stop/continue, detach, and restoration. Gate: nested-PTY process tests with baseline terminal
oracles and no timing thresholds.

### Milestone 4 — first vendor-lane enablement

Re-verify current first-party identity and installed behavior, implement one adapter, and activate
one primary selector only after the full readiness, security, PTY, legacy, and mutation gates pass.

### Milestone 5 — second vendor and structured identity proof

Activate a behaviorally independent second vendor and one concrete structured/ACP descriptor. Gate:
the shared runtime contains no vendor branch, lane isolation holds, and first-vendor behavior remains
unchanged.

### Milestone 6 — rollout and operational gate

Run the deterministic full E2E lane, install/version matrices, upgrade/downgrade compatibility,
artifact cleanup, crash recovery, doctor output, security review, and production registry audit.
Per-lane enablement is reversible descriptor data.

## 14. Non-goals

- Inferring native UI from `isatty`, vendor availability, prompt content, or parent mode.
- Showing a vendor's native subagent UI inside structured, MCP, ACP, automation, or child launches.
- Screen scraping native UI into a transcript or reconstructing structured events from terminal
  bytes.
- Multiple concurrent writers or independent client-controlled PTY geometries.
- Transferring/duplicating the PTY master into clients.
- Automatic vendor installation, login, upgrade, global configuration edits, or credential import.
- Treating ACP, MCP, Gemini, Antigravity, or any other protocol/product names as interchangeable.
- A remote/multi-host pane protocol in the first runtime.
- A dedicated binary pane socket before base64 transport is measured as a bottleneck.
- Preserving arbitrary client resume cursors without session/sequence/checksum validation.
- Making `pty.cast` byte-authoritative for data asciicast cannot encode.

## 15. Trade-offs and future review

The durable sidecar costs another artifact and write path, but it is the smallest boundary that
simultaneously provides byte fidelity, crash recovery, completed replay, and a splice with no
snapshot/listener race. An in-memory history would be simpler but unbounded because terminal modes
may originate in the first bytes; a bounded ring cannot honestly promise late replay. Direct PTY
FD transfer would be faster but breaks supervisor ownership and makes two readers compete.

Base64 on the existing JSON connection favors reuse and correctness over throughput. Revisit a
dedicated Unix pane socket only after queue depth, CPU, and throughput measurements. Revisit resume
cursors when reconnect-from-zero becomes materially expensive, and revisit storage compaction only
with a checkpoint format that captures the complete emulator/mode state rather than dropping the
prefix.

The vendor registry should be reviewed whenever a vendor changes executable identity, injection,
protocol, or UI behavior. Such change invalidates readiness evidence; it does not justify weakening
the explicit mode or terminal ownership invariants.
