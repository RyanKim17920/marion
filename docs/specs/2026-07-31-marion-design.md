# marion — Design

**Status:** design, pre-implementation · **rev 2** (post adversarial review)
**Date:** 2026-07-31
**Companion:** `MILESTONES.md` (goals, principles). This document is the technical design.

> **rev 2 changed the architecture.** Direct-MCP spawn replaces the Agent-tool shim as the
> primary delegation path (§5.4). The Codex app-server reaping requirement is **struck** — it
> did not reproduce (§10). The `Adapter` trait is split because it could not be implemented
> for pty-only modes (§5.2). A security model was added (§7). Claims are version-stamped;
> anything not independently verified is marked **UNVERIFIED**.

---

## 1. Purpose and scope

marion runs any agent harness, on any model, as a first-class subagent of any other harness,
with one UI over the whole tree. Everything serves one primitive:

```
Agent(harness, model, tools, prompt, …) -> handle
handle: observe · steer · interrupt · result
```

Out of scope: the graph-plan system, mesh routing, remote hosting, the north star.

**Verification baseline.** All harness claims below are stamped to: Claude Code **2.1.220**,
Codex CLI **0.145.0**, opencode **1.17.3**, Gemini CLI **0.53.0** (note: earlier research used
0.40.1 — thirteen minors stale; Gemini claims are re-stamped or marked UNVERIFIED). Crates:
`alacritty_terminal` 0.26.0, `pty-process` 0.5.3, `ratatui` 0.30.2, `agent-client-protocol`
2.0.0.

---

## 2. Architecture

```
┌─ marion-supervisor (daemon) ───────────────────────────────┐
│  Registry      nodes, edges, capabilities, ownership       │
│  ControlPlanes per-harness typed control                   │
│  DisplayPlanes pty + VT grid, one per node with a terminal │
│  EventLog      append-only IR, per agent, on disk          │
│  ControlMCP    stdio MCP injected into children (scoped)   │
│  CannedProvider / ModelProxy   (§5.5)                      │
└───────────────▲────────────────────────────────────────────┘
                │ unix socket, NDJSON JSON-RPC 2.0
┌───────────────┴─ marion-tui (client, detachable) ──────────┐
│  Tree · panes · permission+elicitation queue · renderers   │
└────────────────────────────────────────────────────────────┘
```

**Keying.** Supervisor and state are both keyed on the **project root** (the git common-dir,
falling back to cwd) — *not* cwd, because §6.6 worktree children have a different cwd and
would otherwise hash to a different supervisor. Socket path is length-checked against the
104-byte `sun_path` limit; on overflow marion falls back to `/tmp/marion-<uid>/<12-hex>.sock`.

**Client↔supervisor methods:** `tree/subscribe`, `node/get`, `node/attach`, `node/detach`,
`node/input`, `node/steer`, `node/cancel`, `node/kill`, `node/rename`, `permission/reply`,
`elicitation/reply`, `policy/set`, `agent/spawn`, `doctor/run`.

---

## 3. Core concepts

### 3.1 Agent type = launch spec

A declarative record that *compiles* to a process invocation. Markdown + YAML frontmatter
(the prompt is the bulk of the content; every harness already uses this shape).

**Discovery and precedence.** Agent types are loaded from, later overriding earlier:
`$XDG_CONFIG_HOME/marion/agents/*.md` (user) → `<project>/.marion/agents/*.md` (project) →
programmatic definitions passed to the supervisor. The `name:` field is the key and must match
`^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`; the filename is not significant. Duplicate names are an
error at load, reported by `marion doctor`, not silently last-wins.

```yaml
---
name: codex-impl
description: Implements a well-specified change in Rust.
harness: codex
model: gpt-5.3-codex
effort: high
mode: shared
tools: [read, edit, bash]
isolation: worktree
maxTurns: 40
---
You implement changes precisely and do not expand scope…
```

**Tool compilation uses allowlists, not denylists.** Claude Code 2.1.220 ships
`--tools <tools...>` — the actual allowlist. Compiling to `--disallowedTools` would require
enumerating the complement of the built-in tool set and re-deriving it every release, which
guarantees silent privilege escalation the first time a tool is added.

**Tool declarations are only authoritative if the config is minimal.** Copying the user's MCP
config into a child (§6.4) hands that child every MCP tool the user has configured regardless
of `tools:`. Therefore config seeding is **opt-in per agent type** (`inherit_user_config:
true`), default **off**.

### 3.2 Node

```rust
struct Node {
    id: AgentId,                  // marion's own; harness ids are unstable across fork/resume
    parent_id: Option<AgentId>,   // see §7.5 for post-parent-exit semantics
    lamport: u64,                 // causal ordering across nodes (§4.2)
    name: Option<String>,
    agent_type: String,
    harness: Harness,
    harness_version: String,      // resolved at spawn
    binary_path: PathBuf,         // resolved through symlinks (§7.7)
    session: Option<HarnessSessionRef>,   // None for `opaque`
    mode: SpawnMode,
    isolation: Isolation,
    caps: Capabilities,
    state: NodeState,
    depth: u8,
    reap_state: ReapState,        // Live | ReapedIdle | Orphaned  (§7.2)
}
```

### 3.3 Capabilities

Two-stage, because pty-only modes have no handshake to negotiate with:

1. **Static**, keyed `(harness, harness_version, mode)`, produced by `marion doctor` and
   cached. This is the only source for `interactive`/`opaque`.
2. **Refined** at session open for modes with a handshake (ACP `initialize`, app-server
   capability reads), narrowing — never widening — the static set.

```rust
fn static_caps(harness: &Harness, version: &str, mode: SpawnMode) -> Capabilities;
fn refine(&self, session: &Session, base: Capabilities) -> Capabilities;
```

**`SpawnMode` is authoritative over `Capabilities`, not parallel to it.** Mode sets the
ceiling; caps may only be at or below it. `opaque` forces `steer/fork/resume/permissions/
token_deltas/structured_events = false` and `native_tui = true`. `structured_events` is
therefore derived from mode, not stored — removing the overlap the review flagged.

### 3.4 Spawn modes are presets, not the model

The four named modes (`shared` / `headless` / `interactive` / `opaque`, tabled in
`MILESTONES.md`) are a **cross-product of three independent properties**. Encoding them as a flat
enum makes valid combinations silently unrepresentable — e.g. a harness offering typed control
with no display at all would need a fifth mode invented for it.

```rust
enum ControlTransport  { Typed(TypedKind), TerminalInput, LaunchOnly }
enum DisplaySurface    { NativePty, StructuredUi, None }
enum ObservationSource { ProtocolEvents, TranscriptRecords, TerminalBytes }

struct ExecutionSurfaces {
    control: ControlTransport,
    display: DisplaySurface,
    observations: EnumSet<ObservationSource>,   // a node may have several at once
}
```

- `shared` = `Typed` + `NativePty` + `{ProtocolEvents, TerminalBytes}`
- `headless` = `Typed` + `StructuredUi` + `{ProtocolEvents}`
- `interactive` = `TerminalInput` + `NativePty` + `{TranscriptRecords, TerminalBytes}`
- `opaque` = `TerminalInput` + `NativePty` + `{TerminalBytes}`

**Keep the four names** in the UI and in agent-type frontmatter — they are good shorthand and
users should not have to think in three axes. But `ExecutionSurfaces` is what the code branches
on, and it is also what makes §5.2's plane split fall out naturally rather than being a special
case: `DisplayPlane` exists iff `display != None`, `ControlPlane` iff `control != LaunchOnly`.

---

## 4. The event IR

```rust
struct Event {
    agent_id: AgentId,
    parent_id: Option<AgentId>,
    global_seq: u64,          // supervisor-assigned total order (§4.2)
    agent_seq: u64,           // marion's per-agent receive order
    src_seq: Option<u64>,     // source-side sequence where the harness provides one
    caused_by: Option<EventId>,   // explicit causal edge (§4.2)
    thread_id: Option<ThreadId>,  // harness-native correlation, where applicable
    turn_id: Option<TurnId>,
    item_id: Option<ItemId>,
    ts: SystemTime,           // RFC3339 with offset in NDJSON; advisory only
    mono_ns: u64,             // monotonic since supervisor start; used for pty alignment
    provenance: Provenance,
    payload: Payload,
}
```

**`Provenance`** replaces rev 2's `Tier` enum. `Tier` collapsed several independent dimensions
and, worse, implied the live protocol stream is "direct truth" while a transcript is merely
"derived" — which is backwards in an important case: a vendor transcript is often the harness's
*durable source of record*, with stable ids and explicit parent links, and is **more complete
after process death** than an ephemeral notification stream. Exact but delayed is not the same
as lossy.

```rust
struct Provenance {
    source: Source,            // Protocol | Transcript | Pty | Marion
    source_id: Option<String>, // the harness's own id, where one exists
    observed_live: bool,
    authoritative: bool,       // is this the harness's system of record?
    completeness: Completeness,     // Complete | Partial | Unknown
    transformation: Transformation,  // Native | Normalized | Inferred
}
```

`completeness` is what the UI must key on when claiming anything about loss (§4, `src_seq`);
`authoritative` is what reconciliation keys on when the same fact arrives from two sources —
which happens routinely for `shared` nodes emitting protocol events *and* pty bytes at once.

`Payload` variants: `Lifecycle`, `Message{role, block}`, `ToolCall{id,name,kind,input,status,
locations}`, `ToolResult{id,output,is_error}`, `Permission{id,request}`, `Elicitation{id,
request}`, `Plan`, `Usage{tokens,cost}`, `Control(ControlMsg)`, `Raw(Bytes)`,
`Vendor{harness,key,json}`.

`Lifecycle::Spawned` carries `{harness, harness_version, model, agent_type, isolation, caps,
depth, mode}`.

**`seq` records marion's observation order and nothing more.** rev 1 claimed "per-agent `seq`
continuity proves no events were lost" — that was unearned: a notification dropped and never
redelivered simply never gets a number, leaving the sequence gapless. Loss detection requires
`src_seq`, which is populated where the harness supplies one (Codex `item/*` ids, Claude
transcript `uuid` chains) and `None` otherwise. **Where `src_seq` is `None`, marion cannot
detect loss and the UI must not claim otherwise.**

**Granularity is chunks, not messages** — opencode emits token-level deltas, Amp emits whole
messages. A message-granular IR would force buffering and lose live typing.

**`kind: ToolKind`** (from ACP: `read|edit|delete|move|search|execute|think|fetch|switch_mode|
other`) is what lets one renderer draw every harness — Claude's `Edit`, Codex's `apply_patch`,
opencode's `edit`, Gemini's `replace` all normalize to `Edit`.

**`Vendor` is carried, never discarded**; on an ACP wire it serializes into `_meta` (the spec
forbids custom root fields).

### 4.2 Ordering

Wall clock is not trustworthy for causality — NTP steps and sleep/wake move `SystemTime`
backwards, which can invert a parent's spawn against its child's `Spawned`. `ts` is display-only.

**rev 2 proposed a Lamport clock here; that was a phantom solution and is removed.** Lamport
clocks order events across independent producers with no shared sequencer. marion has exactly
one authoritative supervisor receiving and writing every normalized event — it *is* the
sequencer. A monotonic `global_seq` assigned on receipt gives a total order that is exactly as
authoritative, since the same component would have been incrementing the Lamport counter anyway.
It borrowed distributed-systems machinery for a centralized problem and added no information.

Causality that actually matters is represented **explicitly**, not inferred from a counter:
`caused_by` links a child's `Spawned` to the spawn request, a `report` to its task, an approval
response to its request id; `thread_id`/`turn_id`/`item_id` carry harness-native correlation.

A Lamport clock becomes justified only if marion ever admits multiple independent supervisors
generating events offline and merging logs. That is contrary to this architecture; revisit only
if that changes.

`mono_ns` exists to align `events.jsonl` with `pty.cast`: asciicast v3 timestamps are
**relative**, so without a shared anchor the two streams cannot be put on one timeline — which
§8/L6's screen-vs-log oracle requires.

### 4.3 On-disk layout

**`<state>`** = `$MARION_STATE_DIR` if set, else `$XDG_STATE_HOME/marion`, else
`~/.local/state/marion`. **`<project-hash>`** = first 12 hex of BLAKE3 of the canonical project
root (git common-dir, falling back to cwd). Every path below is under
`<state>/<project-hash>/` — including the per-agent config dirs, which §6.4 references by the
short form `<agent-dir>/config/`.

```
<state>/<project-hash>/
  journal.jsonl                       # append-only registry journal (§7.4)
  snapshot.json                       # opportunistic compaction of the journal
  agents/<agent_id>/                  # = <agent-dir>
    meta.json                         # compiled spec, caps, harness ref, binary path+version
    events.jsonl                      # IR, append-only, group-commit (see below)
    pty.cast                          # asciicast v3, modes with a pty
    config/                           # isolated harness config dir, if any (§6.4)
    worktree                          # symlink, when isolation: worktree
```

**Durability is group-commit, not fsync-per-record.** An earlier draft said fsync per record;
with `--include-partial-messages` yielding token-level deltas that is one fsync per token and
would destroy streaming throughput. Instead: append without fsync, fsync on a short timer
(~50 ms), *and* fsync unconditionally before any state transition that must survive a crash —
`Spawned`, `Exited`, `ReapedIdle`, and every journal write. Losing the last few content deltas
in a hard crash costs a slightly truncated replay; losing a lifecycle record costs an untracked
live process. Only the latter pays for a barrier.

**Registry is an append-only journal, not a rewritten `registry.json`.** rev 1 asserted
"atomically rewritten" without designing it, and rewriting the whole tree per state change is
O(tree) per event against an intentionally unbounded tree. The journal is replayed at startup;
a compacted snapshot is written opportunistically via write-temp → fsync → rename → fsync-dir.

**Spawn is journaled before the process starts.** A crash between "process spawned" and
"registry updated" would otherwise leave a live child with no registry entry — an orphan
marion cannot find, holding a session id the ownership invariant no longer knows about.
Intent-then-confirm ordering makes that recoverable.

---

## 5. Components

### 5.1 Registry and session ownership

Neither Codex nor Claude Code locks a session (verified: two concurrent `codex resume`
processes on one id both start, both hold the same inode read/write, neither is refused).
Writes are `O_APPEND` so records survive; the failure is **semantic divergence**, and Codex
rollout records carry `turn_id` but no parent pointer, so the fork is unreconstructable.

The registry refuses to open a harness session id it already holds live. Branching is explicit
(`codex fork`, `claude --fork-session`).

**Scope, stated honestly:** this protects marion from marion. It cannot stop a user opening
the same session in another terminal. It is worth having because reap-and-resume (§7.2),
`send`-to-finished, and orphan recovery are all paths where marion could otherwise
double-open *itself*. A reaped node **keeps** its ownership claim — the id stays held, and
resume goes through the registry.

### 5.2 Planes (was: one Adapter trait)

rev 1's single `Adapter` trait could not be implemented for `opaque`: it required
`events() -> Stream`, `prompt`, `steer`, `interrupt`, and `load(&SessionId)`/`resume(&SessionId)`
for a mode with no event source, no typed input, and **no session id at all**. Split:

```rust
trait DisplayPlane {           // every node with a terminal
    fn spawn_pty(&self, inv: Invocation) -> Result<PtyHandle>;
    fn write_keys(&self, h: &PtyHandle, bytes: &[u8]) -> Result<()>;
    fn resize(&self, h: &PtyHandle, cols: u16, rows: u16) -> Result<()>;
    fn kill(&self, h: &PtyHandle) -> Result<()>;
}

trait ControlPlane {           // only modes with typed control
    fn compile(&self, spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<Invocation>;
    fn open(&self, inv: Invocation) -> Result<Session>;
    fn events(&self, s: &Session) -> impl Stream<Item = Event>;
    fn prompt(&self, s: &Session, p: Prompt) -> Result<()>;
    fn steer(&self, s: &Session, p: Prompt) -> Result<()>;
    fn interrupt(&self, s: &Session) -> Result<()>;
    fn view(&self, id: &SessionId) -> Result<Session>;      // replays history
    fn continue_(&self, id: &SessionId) -> Result<Session>; // does not replay
    fn refine(&self, s: &Session, base: Capabilities) -> Capabilities;
    fn shutdown(&self, s: &Session) -> Result<()>;
}
```

- `opaque` = `DisplayPlane` only. Its events are `Raw(Bytes)` and the supervisor owns it
  directly; it is not a `ControlPlane` implementor and does not pretend to be.
- `interactive` = `DisplayPlane` + a **read-only** `ControlPlane` (transcript tail supplies
  `events()`; `prompt`/`steer` route to `write_keys`; `interrupt` is a signal).
- `headless` = `ControlPlane` only. `shared` = both, fully.

**`view` / `continue_` replaces `load` / `resume`.** The distinction is marion-native —
rebuild a view (orphan recovery, §7.2) versus continue work (reap-resume, §7.2) — and is *no
longer justified by citing ACP*, because ACP v2's method list drops `session/load` entirely. `agent-client-protocol`
2.0.0 is still wire **v1** (v2 is behind `unstable_protocol_v2`), so v1 semantics hold today;
the trait is named so that it survives v2.

**Day-one:** claude-code, codex. Then acp (breadth), opencode.

**codex.** `shared` mode: `codex app-server --listen ws://IP:PORT` (a literal `SocketAddr` —
a hostname is a hard `InvalidWebSocketListenUrl`, and `wss://` is rejected). Threads via
`thread/start`, driven with `turn/start`/`turn/steer`/`turn/interrupt`, consuming `item/*`.
`codex --remote` attaches a TUI in a pty on demand (interactive subcommands only).

**`thread/resume` — corrected.** rev 2 said "never use it on live threads," which was too
broad. It is a **load-from-persisted-history** operation, not an attach primitive:

- **Cold thread on disk:** `initialize` → `initialized` → optionally `thread/read` (expect
  `notLoaded`) → `thread/resume` → `turn/start` → consume to `turn/completed`.
- **Live in-memory thread:** keep the subscription from `thread/start`/`thread/resume`,
  optionally `thread/read` to refresh, then `turn/start`. **Do not** call `thread/resume` merely
  to attach — a live thread can exist in the app-server's registry before its rollout is
  materialized, so resume enters the persisted path and fails (`no rollout found`) even though
  the thread is perfectly alive. That failure is not evidence the thread is gone.

`notLoaded` is a **residency** status, not a persistence verdict. Statuses: `active` (loaded,
work in progress), `idle` (loaded, no active turn), `systemError`, `notLoaded` (known as stored
history, no live in-memory session here).

> **S5 RESOLVED (2026-07-31) — PASS. `shared` mode works.** Verified against 0.146.0 and
> confirmed in `rust-v0.146.0` source. **`thread/resume` IS the subscribe mechanism; there is no
> `thread/subscribe`** (the server's own 125-method enumeration lists `thread/unsubscribe` and no
> inverse). Subscribers live in `ThreadEntry { connection_ids: HashSet<ConnectionId> }`
> (`app-server/src/thread_state.rs:276`), inserted by `thread/start`, `thread/fork`,
> `thread/resume` (both cold **and already-loaded** paths — `thread_lifecycle.rs:647`),
> `review/start`, `thread/realtime/start`. **Not** by `thread/read` (its handler isn't even passed
> a `ConnectionId`) and **not** by `turn/start`.
>
> Measured: a late joiner with `thread/read` alone received **2** events (global broadcasts only,
> zero `item/*`); after `thread/resume`, **11 — full parity** with the originator, who was
> undisturbed. Originating a turn does *not* subscribe you. Fanout re-reads the subscriber set per
> event, so **mid-turn attach works** — a client joining 2.6 s into a streaming turn received 87
> of 93 subsequent events including live deltas, without interrupting it. Multiple concurrent
> subscribers work, including after the originating connection closes. `thread/unsubscribe` is
> per-connection and does not affect other subscribers or the thread.
>
> **marion's attach sequence:**
> ```
> initialize {clientInfo}  →  initialized
> thread/read   {threadId, includeTurns: true}   // backfill; does NOT subscribe
> thread/resume {threadId}                       // subscribes; additive, non-disruptive
> turn/start | turn/steer | turn/interrupt
> thread/unsubscribe {threadId}                  // clean detach
> ```
> Order matters: resume delivers **no replay**, so `thread/read` must come first or events between
> attach and subscribe are lost.
>
> **Caveats marion must encode:**
> 1. `thread/resume` config overrides (`model`, `sandbox`, `approvalPolicy`) are **silently
>    ignored** on an already-loaded thread (`thread_processor.rs:3529`, warn-only). Never rely on
>    them in an attach-resume.
> 2. Resuming a thread with **zero** subscribers that is idle-and-not-running triggers a shutdown
>    and cold re-resume (`thread_processor.rs:3495`). Attaching to a merely-cached thread is *not*
>    a pure attach; attaching to one a TUI still holds is.
> 3. Retryable error: `"thread {id} is closing; retry thread/resume"`. Hard errors: `history` +
>    already-running, stale `path` mismatch.
> 4. `experimentalRawEvents` is thread-global, sticky, and settable **only** on `thread/start` —
>    resume always passes `false` and cannot clear it. If marion wants raw events it must
>    originate the thread.
> 5. `initialize` accepts `optOutNotificationMethods: string[]` — use it to mute broadcast noise
>    on observer connections.
> 6. **Approvals and elicitations fan out to *all* subscribers as server→client requests, and
>    whoever answers first wins.** So marion attaching to a TUI-owned thread receives duplicate
>    approval prompts racing a human. **Policy: marion answers approvals only on threads it
>    originated; on attached threads it renders them read-only and lets the owning UI decide.**
>    Use `approvalsReviewer` on turns marion originates.
>
> Fixtures: `tests/fixtures/s5/` — three probes plus the server's method enumeration.

**`turn/completed` is the authoritative turn-termination signal.** Nothing else is:
`item/completed` ends one item of possibly many; an `error` event is diagnostic, not a lifecycle
boundary; `thread/status/changed → idle` is thread-level and can lag, race, or coalesce; a
closed transport cannot distinguish completion from interrupted delivery, server death, or an
updater replacement; and a successful `turn/interrupt` means the *request* was accepted, not
that cancellation finished. Relying on anything else means returning before the answer is known,
scoring interrupted turns as successful, or starting a turn while Codex still considers the
previous one active.

**Server-initiated approval requests are JSON-RPC *requests*, not notifications** — they carry
an `id` and **block the turn** until answered:
`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`,
`item/permissions/requestApproval`, plus `item/tool/requestUserInput`,
`mcpServer/elicitation/request`, and `item/tool/call` for client-executed tools. Respond with
the same id: `{"id":41,"result":{"decision":"accept"}}` — also `acceptForSession`, `decline`,
`cancel`. `serverRequest/resolved` follows. So the codex `ControlPlane` needs a continuously
serviced bidirectional reader and an id→pending-decision map, exactly as the Claude adapter does
(§5.2, S1) — and it needs a stated policy for when no human UI is attached, or turns hang.

**`codex exec --json` is the better surface for one-shot children** — a bounded job with one
prompt and one terminal result, no steering, no attached TUI. It removes the handshake, thread
loading, subscriptions, bidirectional approvals, item accumulation, and long-lived server
lifecycle. Useful flags: `--output-schema`, `--output-last-message`, `--cd <worktree>`,
`--sandbox workspace-write`, `--ephemeral`, `--ignore-user-config`. **Prefer it for fan-out
work; reserve app-server for genuinely interactive children.**

> **S3 RESOLVED (2026-07-31). Idle app-servers are never reaped — no process heartbeat needed.**
> Six invocations (bare `--listen`, `daemon start`, orphaned, each with and without a live thread
> and a held client) all survived 43 minutes. Two source sweeps of `rust-v0.145.0` found no timer
> in the 86–90 s band. The only app-server killer is `app-server-daemon`'s `PidBackend::stop`,
> which targets **only** the start-time-verified pid in its pidfile and explicitly refuses an
> unmanaged server.
>
> **The original phantom SIGTERM was most likely self-inflicted:**
> `codex-app-server-test-client`'s `kill_listeners_on_same_port` (`lib.rs:687`) runs
> `lsof -tiTCP:<port>` and kills whatever it finds, with no delay floor — matching every symptom
> including death while SIGSTOPped.
>
> The **updater is real but exonerated**: `INITIAL_UPDATE_DELAY` 300 s / `UPDATE_INTERVAL` 3600 s
> (`update_loop.rs:46,50`), so it cannot fire at 86–90 s, and it is installed only by
> `daemon bootstrap` / `remote-control start`, never `daemon start`. Confirmed by natural
> experiment: a daemon server on 0.145.0 was **not** restarted when the package swapped to
> 0.146.0 mid-spike. `REMOTE_CONTROL_CLIENT_IDLE_TIMEOUT` (600 s) drops a *relay registration*
> only, applies to the outbound ChatGPT transport, and a server survived 763 s with an idle client
> attached. `shutdown_when_no_connections` is gated to **stdio only**, so a ws/unix server does
> not exit when its last client leaves.
>
> **⚠ The genuine hazard is `THREAD_UNLOADING_DELAY = 1800 s`.** `thread/start` returns a rollout
> `path` but **does not create the file**, and an **unsubscribed** thread is unloaded after 30
> minutes on a perfectly healthy server — measured: `thread/read` OK at 1241 s, then
> `thread not loaded` / `no rollout found` at 1962 s, identical across a restart. **Read-only
> probes do not refresh the timer, and a connection heartbeat would not have helped — the timer is
> on the thread, not the connection.**
>
> **Therefore marion must:** run a **bare `--listen ws://` app-server it owns** (immune to every
> codex-rs kill path); **never** run `daemon bootstrap` or `remote-control start` (note
> `daemon stop` does *not* stop the updater, which then restarts the server, and there is no
> config key or env var to disable it); and for any thread that matters either **keep a subscriber
> attached** (`thread/resume`, per S5), **materialize its rollout by running a turn**, or be able
> to re-create it. A never-turned, unsubscribed thread is lost after 30 minutes.
>
> Fixtures: `tests/fixtures/s3/`. **Caveat:** `daemon bootstrap` was not run (it installs durable
> user state), so the updater's kill path is source-verified, not measured.

**claude-code.** `interactive` (pty + JSONL tail) or `headless`
(`-p --output-format stream-json --input-format stream-json`). Liveness via
`claude agents --json` (no TTY needed). Note `claude attach <interactive-id>` prints
`No job matching…` **and exits 0** — never branch on its exit status.

> **S1 RESOLVED — PASS (2026-07-31).** Verified against 2.1.220 by decompiling
> `@anthropic-ai/claude-agent-sdk@0.3.220`, confirming from the binary's strings, and replaying
> from plain Python with no SDK. **No TS sidecar needed.** Protocol, NDJSON on stdin:
>
> ```json
> {"type":"user","session_id":"","message":{"role":"user","content":[{"type":"text","text":"…"}]},"parent_tool_use_id":null}
> {"type":"control_request","request_id":"req_2","request":{"subtype":"interrupt"}}
> ```
>
> `session_id:""` is what the SDK literally sends — the CLI owns the id and reports it on
> `system/init`. `request_id` is client-generated and any unique string works. Optional
> `cancel_queued: true` inside `request` also drops queued commands. Reply arrives on stdout as
> `{"type":"control_response","response":{"subtype":"success","request_id":…,"response":{"still_queued":[]}}}`
> in **1 ms**; the terminal `result` follows in **2 ms**. `initialize` is **optional** — the
> interrupt path works with no handshake; it is needed only to register SDK-side hooks/MCP or
> read the session's command/agent/model catalogue.
>
> **The channel is bidirectional, and this is load-bearing.** The CLI emits its *own* outbound
> `control_request` frames — `can_use_tool`, hook callbacks, `request_user_dialog` — on the same
> stdout stream, expecting marion to answer with a `control_response`. So `ControlPlane` needs a
> `request_id -> oneshot::Sender` demux map, and **this is the concrete mechanism by which Claude
> Code permission prompts reach marion's permission queue** (§5.6). Cancel an in-flight request
> with `{"type":"control_cancel_request","request_id":…}`.
>
> **An interrupted turn reports `is_error: true`** with `subtype:"error_during_execution"` and
> `terminal_reason:"aborted_streaming"`. marion MUST classify that as a clean interrupt, not a
> failure.
>
> Other verified details: `--include-partial-messages` yields token-level
> `{"type":"stream_event","event":{"type":"content_block_delta",…}}` but **requires `--verbose`**
> alongside `-p --output-format stream-json`. A fresh `system/init` frame is emitted **per turn,
> not per process** — do not treat it as a new session. Do not gate on the `initialize` response
> advertising `capabilities` (2.1.220 returns none while still honoring `still_queued`); treat a
> missing `still_queued` as the older-CLI fallback.
>
> **UNVERIFIED:** the replay used pipes, not a pty. If the CLI does isatty-conditional line
> buffering, behavior under `pty-process` may differ. Fixture: `tests/fixtures/s1/`.

### 5.3 Display plane: pty + VT

`pty-process` 0.5.3 with `features = ["async"]` (default is `[]`) — native tokio
`AsyncRead`/`AsyncWrite`, `setsid` + `ioctl_tiocsctty`, real `resize`. **Unix-only, no
`cfg(windows)` anywhere.** `portable-pty` 0.9.0 is blocking-only (`std::io::Read`/`Write`), so
Windows support is a thread-bridge plus a second I/O model behind the trait — real work, not a
feature flag. Windows is deferred.

`alacritty_terminal` 0.26.0 for the VT. **The rev 1 rationale for rejecting `vt100` was
wrong.** Both crates drop scrollback under a scroll region whose top is not row 0:

- `alacritty` `grid/mod.rs:271-301` rotates into history only `if region.start == 0`, else
  swaps rows (dropped); `:258` resets rows outright when the region is smaller than the scroll
  amount and `region.start != 0`.
- `vt100` `grid.rs:566` keys on `scroll_top != 0 || scroll_bottom != rows-1`.

So the real difference is **top-anchored vs top-or-bottom**: with `ESC[1;20r` (bottom status
bar) alacritty keeps history and vt100 does not; with `ESC[2;24r` (top offset) **neither
does**. alacritty remains the pick — it is strictly more capable, is published (unlike
`wezterm-term`), and handles OSC 8 (parsed in `vte` 0.15 `ansi.rs`, surfaced per-cell) and
DECSET 2026. But the choice does not by itself deliver scrollback.

> **S2 must first measure which DECSTBM shape Claude Code and Codex actually emit.** If either
> uses a top offset, no off-the-shelf crate gives us scrollback and we either patch alacritty
> or maintain our own history above the scroll region. rev 1's E0 pass criterion
> ("alt-screen-free scrollback correct") was unachievable-by-default and is now conditional.

**`renderable_content()` is viewport-only** (`display_iter` runs `-display_offset-1` to
`bottommost_line()`). Scrollback requires `Grid` indexing with negative `Line` or driving
`scroll_display()`. The rev 1 "~150-line adapter" estimate covered the viewport half only.

Coverage needed: SGR incl. 24-bit, CUP/ED/EL, DECSET 2026, reverse index, DECSTBM, OSC 0,
OSC 8, cursor save/restore, `?1049h/l` (alt screen), `CSI 3J`, and mouse modes
`?1000/?1002/?1003/?1006`. A dumb host answering **no** probes was verified to run both TUIs
correctly.

> **S2 RESOLVED (2026-07-31) — it corrected two claims and moved the hazard.**
>
> **Claude Code 2.1.220 DOES use the alternate screen** (`ESC[?1049h` at startup, `?1049l` at
> exit, plus mouse tracking). Earlier text saying neither harness does was stale. Consequence:
> **for Claude Code there is no scrollback to retain** — the session is a fixed viewport,
> `renderable_content()` suffices, and the ~150-line adapter estimate holds for that path.
> **Scrollback is a Codex-only concern.** Codex uses the main screen, entering alt screen only
> for transient overlays (the `/diff` pager, properly paired).
>
> **DECSTBM: PASS — though the literal assert was the wrong test.** Codex *does* emit top-offset
> regions (`ESC[9;24r`, `ESC[8;40r`, `ESC[17;40r`), but *exclusively* paired with reverse index —
> scrolling **down**, which never produces history in any terminal. Every scroll **up**, the only
> operation that feeds scrollback, occurs under `top == 1`. Confirmed by replaying real captures
> through both crates: `alacritty_terminal` retained 94 lines of genuine content from a 14-row
> Codex run; **`vt100` retained 0 in every stream** — confirmed unusable.
>
> **The real hazard is `CSI 3J`.** Codex emits `ESC[r ESC[0m ESC[H ESC[2J ESC[3J ESC[H` on every
> resize. `CSI 3J` is *erase scrollback*, which alacritty honors via `clear_history()`, so history
> drops to zero on **every SIGWINCH** — this took one capture from 121 lines to 2.
> **Decision: marion intercepts `CSI 3J` and maintains its own append-only history.** The harness
> clears scrollback because it is about to repaint a viewport, not because the transcript is
> invalid — and marion's entire value is that the transcript outlives the display.
>
> **DECSET 2026 validated as the "assert now" signal** (§8/L4.5): strict `h`/`l` alternation,
> zero violations across every capture, every bracket closed, every frame containing a CUP. In
> Codex, 16/18 and 22/24 DECSTBM changes occur *inside* a 2026 bracket, so a synchronized-output
> boundary never observes a half-applied scroll region.
>
> **Untested edge:** alacritty `grid/mod.rs:258` resets rows outright when a scroll exceeds a
> `region.start != 0` region's height. Observed RI bursts never came close (9 lines in a 33-row
> region) and Codex chunks large inserts into repeated top-anchored passes — but this is unproven
> for a single huge tool output on a very short terminal.
>
> Fixtures: `tests/fixtures/s2/` — 5 captures as asciicast v3 + raw, plus analysis tools.

**Keystroke injection rules** (verified): never send text and `\r` in one write (paste-burst
heuristics swallow the submit); never wrap in bracketed paste; one line at a time; wait for
boot modals detected from screen state.

### 5.4 Control MCP — direct spawn is the primary path

rev 1 made the Claude Code Agent-tool shim primary. **That was backwards on fidelity and on
cost**, and the review is right:

- The built-in Agent tool's return value is *the shim subagent's own final text*. So a real
  child's structured result reaches the parent only after the shim **retypes it as prose** —
  paraphrasable, truncatable, hallucinable. That is precisely what §7.6 exists to prevent, and
  the shim reintroduces it structurally. rev 1's claim that the parent reasons over the
  structured result "with zero adaptation" was **false**: the parent sees a string.
- Each shim is a full Claude Code process. At ~462 MB for an active session, a 53 MB Codex
  child costs ~9× more with a shim in front of it, which invalidated rev 1's capacity numbers.

**Primary path: the parent calls `mcp__marion__spawn` directly and receives the structured
result as a genuine tool result.** One turn, no retyping, no extra process, full fidelity.

The Agent-tool shim remains available as **optional ergonomic sugar** for users who want
foreign agents to appear in Claude Code's native agent picker, with its fidelity and memory
costs documented. It is not on the critical path.

| tool | purpose |
|---|---|
| `spawn` | create a child node; returns structured result (or a handle if backgrounded) |
| `send` | message a node — see authorization below |
| `report` | explicit result return (§7.6) |
| `status` / `wait` / `cancel` / `list` | node state, block-until-idle, interrupt, discovery |

**Authorization (new in rev 2).** rev 1 handed `spawn`/`send`/`cancel`/`list` to every child
with no scoping — a peer routing table with an LLM on both ends, i.e. the mesh the star
topology explicitly forbids, and a prompt-injection channel between siblings. Now:

- Every child gets a per-node **capability token** bound to its `AgentId`.
- `send`, `cancel`, `status`, `wait` are permitted **only to the node's own descendants**, or
  to its parent. Sibling addressing is denied by default.
- `list` returns descendants and parent only.
- Cross-branch addressing requires an explicit `allow_peers: [names]` grant in the agent type.
- Every denied call is logged and surfaced in the UI — a child attempting lateral addressing
  is a signal worth seeing.

**Wiring.** The server is registered under the name `marion`, producing the `mcp__marion__*`
prefix Claude Code and Gemini apply. It is injected per child by the fileless path where
available (`--mcp-config '{"mcpServers":{"marion":{...}}}'` for Claude Code; `-c
mcp_servers.marion={...}` for Codex; `OPENCODE_CONFIG_CONTENT` for opencode), else written into
`<agent-dir>/config/`. The command is `marion-supervisor mcp --token <tok>`, a thin stdio bridge
back to the supervisor socket. The capability token is passed **as an argv flag on that bridge
command, not as an env var**, so it is not inherited by grandchildren or by tools the agent
shells out to. The bridge resolves the token to an `AgentId` and stamps every call, so a child
cannot address outside its grant even if it reads its own config.

`report`'s payload mirrors Claude Code's Agent result shape (`totalTokens`, `totalDurationMs`,
`totalToolUseCount`, `usage`, `toolStats`, `worktreePath`) — and on the direct-MCP path the
parent genuinely receives it.

### 5.5 Canned provider and model proxy — two components, not one

rev 1 bundled these and got the sequencing wrong in both directions — it called the proxy
"optional" while making it the foundation of E2E testing, then scheduled it last.

- **CannedProvider (early, small, unblocking).** Replays scripted SSE on a single wire format
  (Anthropic Messages first) for L4. Port Codex's `mock_model_server.rs` pattern — `wiremock`
  + `SeqResponder` + `.expect(n)` — and `core_test_support::responses` builders rather than
  inventing an event vocabulary.
- **ModelProxy (late, genuinely large).** Translation across four wire formats for
  any-harness × any-model. Build order by measured difficulty: opencode (no proxy needed —
  speaks all four natively) → Claude Code → Qwen → Gemini → **Codex last** (Responses API
  only; `wire_api="chat"` was removed). Amp is structurally blocked.

marion need not *write* the translation — LiteLLM, Vercel AI Gateway, and OpenRouter do it.
marion owns the launcher primitives and the canned mode. Security constraints in §7.1.

### 5.6 TUI client

Tree pane, content pane, permission **and elicitation** queues. Per-harness renderer plugins
keyed on `(harness, vendor_key)`, generic widgets as fallback.

marion is the only process seeing permission requests from every harness — one queue, one
keybinding, central policy. An `opaque` node cannot participate and will block invisibly, so
the UI shows **"possibly blocked, no permission channel"** with elapsed time, never a spinner.

---

## 6. Data flow

### 6.1 Spawn

1. Parent calls `mcp__marion__spawn { agent_type, prompt, name? }`; token is checked (§5.4).
2. Resolve agent type; check depth, concurrency caps, and **write-conflict policy** (§6.6).
3. Resolve the harness binary **through symlinks**; record path + `--version`.
4. Isolation: `worktree` → create; `shared-cwd` → inherit.
5. `compile()` → argv + env + config, into an isolated config dir if needed (§6.4).
6. **Journal the spawn intent**, then start the process, then journal confirmation.
7. `Lifecycle::Spawned` with static caps, refined if the mode has a handshake.
8. Events stream into the EventLog immediately and continuously, watched or not.

### 6.2 Observe

"Opening" a node is a **view switch in the client**, never a connection event. The supervisor
has held the channel since `t=0`, so the double-open hazard is structurally unreachable.

### 6.3 Steer vs continue

rev 1's §5.4 collapsed what its own §6.3 forbade collapsing. Now explicit and separate:

- **`node/steer`** — mid-flight injection into a **running** node. Requires `caps.steer`.
- **`node/prompt`** — a new turn on an **idle** node.
- **`send` to a finished node** — supervisor performs `continue_()` then `prompt()` as one
  atomic registry operation, so there is no race between the two calls.

### 6.4 Config injection and isolation

marion **never mutates the user's real harness config**.

- **Fileless preferred:** Claude Code `--agents '<json>'` + `--mcp-config`; Codex `-c
  key=value` (global, repeatable, TOML-parsed); opencode `OPENCODE_CONFIG_CONTENT`.
- **Isolated dir otherwise:** `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `GEMINI_CLI_HOME` under
  `<state>/agents/<id>/config/`, cleaned up on node deletion.

> **⚠ `CLAUDE_CONFIG_DIR` isolation breaks OAuth authentication.** Found incidentally during S4:
> the macOS Keychain entry holding Claude Code's OAuth credentials is keyed to the **real** config
> dir, so a child launched with an isolated `CLAUDE_CONFIG_DIR` cannot authenticate — S4 had to
> route through a local Anthropic-compatible proxy to run at all. Consequences:
> - Config isolation and subscription auth are **mutually exclusive** for Claude Code children.
> - Options, none free: **(a)** use the fileless path (`--agents`, `--mcp-config`, `--settings`),
>   which keeps the real config dir and therefore keeps auth — **now the strong default, not
>   merely the preference**; (b) copy credential material into the isolated dir, multiplying the
>   blast radius §7.1 exists to contain; (c) run isolated children on an API key or through
>   marion's proxy, accepting different billing.
> - Any design element that *requires* an isolated `CLAUDE_CONFIG_DIR` must state which of
>   (a)/(b)/(c) it takes.
> - **UNVERIFIED:** whether `CODEX_HOME` and `GEMINI_CLI_HOME` carry the same coupling. Check
>   before relying on isolation for those.
- **Seeding from the user's real config is opt-in** (`inherit_user_config`), default off,
  because those directories hold OAuth tokens and API keys and because inherited MCP servers
  silently defeat `tools:` (§3.1). When on, marion copies only the keys the agent type names.

**Verified launcher requirements (2.1.220 / 0.145.0):**

- `ANTHROPIC_API_KEY=""` when using `ANTHROPIC_AUTH_TOKEN` — a non-empty key silently wins.
  Note the empty string is inherited by grandchildren and by shelled-out tools, and some SDKs
  distinguish empty from absent. Set it as narrowly as possible.
- `CLAUDE_CODE_ATTRIBUTION_HEADER=0` — a per-request nonce destroyed third-party prefix
  caching (0% → 99.7% when stripped).
- `127.0.0.1` not `localhost` for Codex `--listen`; give `unix://` its own directory.
- Codex reserves provider ids `openai`, `ollama`, `lmstudio`, `amazon-bedrock`.
- Both binaries require a real TTY.
- **`CLAUDE_CODE_CHILD_SESSION` — corrected.** rev 1 said scrubbing it was mandatory or no
  transcript is written. A/B testing at 2.1.220 wrote transcripts **both** ways. The real gate
  also requires the interactive path, not-a-teammate, and no tmux marker; `-p` is unaffected;
  a third variable `CLAUDE_CODE_SKIP_PROMPT_HISTORY` has its own path. Claude Code also *sets*
  this variable itself when spawning children, so scrubbing changes how the child identifies
  itself. **Policy:** set `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` for `interactive` children
  (explicit, no side effects) rather than scrubbing.

### 6.5 Result

Explicit only (§7.6), returned as a structured tool result on the direct-MCP path.

### 6.6 Concurrency and isolation

marion creates worktrees and reports diffs. **It never auto-merges** — that is how multi-agent
systems silently destroy work. Merging is an explicit act by the parent or user.

**`shared-cwd` write conflicts (new in rev 2).** rev 1 offered `shared-cwd` and analyzed only
merging, which `shared-cwd` doesn't do. Two children writing one tree is lost-update, and it is
worse than two humans because each harness keeps its own checkpoint state — a checkpoint
restore in one child silently reverts the other's work.

Policy: **at most one node with write tools per cwd by default.** A second write-capable spawn
into an occupied cwd is refused with a message naming the holder, and the caller must either
wait, use `isolation: worktree`, or pass an explicit `allow_concurrent_writes` override.
`ToolCall.locations` gives attribution, but attribution is forensics — this is prevention.

---

### 6.7 The task contract

Introduced after the independent Codex review, which argued — correctly — that marion's durable
value is not "display several agents" but **"delegate repository work across independently
evolving executors and know exactly what came back."** A prose result from a foreign agent is
not auditable. A task contract is.

Every delegation records one, written at spawn and completed at result:

```rust
struct TaskContract {
    task_id: TaskId,
    requester: AgentId,                  // parent identity
    child: (Harness, String),            // harness + resolved version
    repo: RepoIdentity,                  // git common-dir
    base_commit: Oid,
    workspace: Workspace,                // worktree path + branch, or shared-cwd
    instructions: String,
    acceptance_criteria: Vec<String>,    // stated up front, never authored after the fact
    allowed_tools: Vec<String>,
    writable_scope: Vec<PathBuf>,        // declared, then checked against ToolCall.locations
    timeout: Option<Duration>,
    verification: Vec<Command>,          // commands that must pass

    // filled on completion
    status: ResultStatus,                // Ok | Failed | Cancelled | Unreported | TimedOut
    narrative: Option<String>,
    result_commits: Vec<Oid>,
    changed_paths: Vec<PathBuf>,
    diff: Option<Patch>,
    evidence: Vec<CommandOutcome>,       // verification runs, with exit codes and output
    exit: ProcessExit,
    timestamps: TaskTimestamps,
}
```

Two rules make it more than bookkeeping:

- **`acceptance_criteria` and `verification` are authored at spawn time, before the child runs.**
  Criteria written afterward describe what happened, not what was required — the failure mode
  from `info.md`'s original notes.
- **`writable_scope` is checked against observed `ToolCall.locations`.** A child that writes
  outside its declared scope is reported, not silently accepted. This converts §6.6's
  attribution from post-hoc forensics into an actual check.

The contract is what a parent gets back from `mcp__marion__spawn` (§5.4), what the UI renders as
a completed node, and what makes a run replayable. It is deliberately independent of harness: a
Codex child and a Claude child return the same structure.

> This is also the seam where `info.md`'s graph-plan system would later attach — a plan node is a
> task contract with dependencies. Still out of scope (see `MILESTONES.md`), but the contract is
> designed so that adding it later is not a rewrite.

---

## 7. Security and failure modes

### 7.1 Security model (new in rev 2)

**Threat model.** Children run arbitrary code by design; they are not a trust boundary. marion
protects (a) the user's credentials, (b) the user's source, and (c) nodes from each other.

- **Model traffic.** The proxy and canned provider bind **loopback only**, on an ephemeral
  port, with a per-run bearer token in the child's env. Never `0.0.0.0`.
- **Credentials never transit marion's logs.** The proxy redacts `Authorization`,
  `x-api-key`, `anthropic-*` auth headers, and any configured secret pattern before anything
  is written to disk.
- **Fixtures are the biggest leak risk in this design, and it is self-inflicted.** §9 requires
  every spike to emit a fixture and §8/L4 replays recorded conversations — which contain system
  prompts, full repo contents in tool results, and anything secret that appeared in output.
  Therefore: recording runs through a **redaction pass** that scrubs headers and known secret
  shapes; fixtures land in `tests/fixtures/` with a mandatory `REVIEW.md` checklist; and a
  pre-commit hook (gitleaks or equivalent) blocks a commit containing fixture files that
  fail the scan. **Fixtures recorded against a real provider are never committed without a
  human read.** Prefer recording against the canned provider.
- **Control MCP** is scoped per node (§5.4). Descendants and parent only, by default.
- **Supply chain.** If LiteLLM is ever used it is pinned by hash and run out-of-process; it
  shipped credential-stealing malware on PyPI 1.82.7/1.82.8, so version pinning alone is not
  an answer for a component in the credential path. Default is marion's own canned provider.
- **Config copies** are opt-in (§6.4) and deleted with the node.

### 7.2 Reaping, orphans, and supervisor restart

`reap_state` distinguishes what rev 1 conflated — after a supervisor crash, a deliberately
reaped node and a killed orphan were indistinguishable on disk, so restart recovery would have
marked perfectly resumable nodes as dead.

- **`ReapedIdle`** — process killed to reclaim memory, transcript intact, ownership claim
  retained, resumable. Written to the journal *before* the kill.
- **`Orphaned`** — process lost without a recorded reap (supervisor crash). Marked on restart
  only for nodes that were `Live`.
- Running nodes are never reaped. SIGSTOP is not used as hibernation: measured, it saves no
  memory (footprints unchanged across a 175s stop, zero swapouts). The one place it pays is
  `opencode serve`, which busy-polls at 1.15%/core while idle.

On restart: replay the journal, mark `Live` → `Orphaned`, offer `view()` replay where the
harness supports it, and leave `ReapedIdle` nodes resumable.

### 7.3 TUI dies

Nothing happens to agents. Reattach replays `events.jsonl` per node.

### 7.4 Journal corruption

A truncated final line is discarded on replay (append-only, fsync per record). Snapshots use
write-temp → fsync → rename → fsync-dir. Spawn is journaled as intent-then-confirm so a crash
mid-spawn is recoverable rather than producing an untracked live process.

### 7.5 Parent exits while a child lives

Explicit, since rev 1 left it undefined. `parent_id` is **immutable** — the tree never
silently re-parents. A child whose parent has `Exited`:

- keeps its edge (the tree records history, including dead nodes);
- is marked `orphaned_report: true`;
- on `report`, the result is stored and surfaced in the UI as **unclaimed** rather than
  delivered, since there is no live turn to return into.

Re-parenting on request is a future affordance, never automatic.

### 7.6 Agent stops without reporting

1. Agent types are prompted to call `mcp__marion__report`.
2. On stop without a report, marion re-prompts once: *are you reporting a result, or waiting?*

> **S4 RESOLVED (2026-07-31) — PASS on both harnesses.** Use
> **`{"decision":"block","reason":"…"}` on both** — one code path. On Claude Code the reason
> arrives as a real `user` message (`Stop hook feedback:\n<reason>`), `num_turns` goes 1→2, and it
> is **observable in `stream-json`**. Exit-code-2-plus-stderr is equivalent.
>
> **Do not use `additionalContext`** (rev 2 specced it): on Claude Code it does produce another
> turn but is delivered as a system-reminder emitting **no stream event**, leaving `num_turns` at
> 1 — marion would have to diff the transcript to know the re-prompt landed. On **Codex it does
> not exist**: `stop.command.output` is `additionalProperties:false` over
> `{continue, decision:["block"], reason, stopReason, suppressOutput, systemMessage}`.
>
> **`stop_hook_active` is the loop guard** — `false` on first fire, `true` on the second, on both
> harnesses. Honor it and the re-prompt happens exactly once.
>
> Stop-hook input is rich enough to decide without parsing a transcript: `session_id`,
> `transcript_path`, `cwd`, `permission_mode`, `stop_hook_active`, **`last_assistant_message`**,
> plus `background_tasks`/`session_crons` (Claude) and `turn_id`/`model` (Codex).
>
> **⚠ Codex hooks are trust-gated and fail *silently*** — no warning, no log — until trusted.
> marion must bootstrap trust by writing `$CODEX_HOME/config.toml`:
> ```toml
> [hooks.state."<sourcePath>:<event_snake>:<groupIdx>:<hookIdx>"]
> enabled = true
> trusted_hash = "sha256:…"
> ```
> Key and current hash come **non-interactively** from the app-server: `initialize` →
> `initialized` → `hooks/list {"cwds":[…]}` returns `key`, `currentHash`, `trustStatus`. Tie hash
> invalidation into §7.7's version-bump handling.
>
> **⚠ Branch on `hook_event_name`.** Returning the Stop-shaped `{"decision":"block"}` from a
> `UserPromptSubmit` hook **blocks the user's prompt entirely** and the turn produces no output.
>
> Other Codex gotchas: `hooks.json` accepts `Stop` and `stop`; **unknown event keys are silently
> ignored**; `matcher` is ignored for `Stop`; shape is
> `{"description":…,"hooks":{"<Event>":[{"hooks":[{…}]}]}}`.
>
> **`SubagentStop` verified statically only** — in the 2.1.220 bundle it shares the `Stop` code
> path exactly, and its input adds `agent_id`, `agent_type`, `agent_transcript_path`, directly
> useful for marion's child nodes. **A live confirmation under real auth is owed before M1.**
>
> Fixtures: `tests/fixtures/s4/{claude-code,codex}/`. The "one more turn on marion's own channel"
> fallback is unnecessary for these two harnesses; retained for future ones.
3. Still nothing → synthesize from the transcript tail, mark `Exited{Unreported}`, surface
   visibly. **Never silently promote a status message to an answer.**

### 7.7 Harness auto-update mid-session

`claude update` and `codex update` exist, and Claude Code's `~/.local/bin/claude` is a
**symlink** into `versions/<ver>` that a background update repoints while children run.
Therefore: `binary_path` is resolved through symlinks at spawn and pinned in `meta.json`;
`harness_version` rides on `Lifecycle::Spawned`; and a node resumed under a different version
than it was recorded with is flagged, with its cached caps invalidated.

### 7.8 Someone else's supervisor

`claude daemon stop` terminates background sessions and has `--any` / `--keep-workers`. marion
does not depend on that daemon, but must tolerate it acting on marion's children: an
unexplained child death is `Exited{Killed}` with "external termination" and never presented as
a normal completion.

### 7.9 Transcript hazards

- Claude Code transcripts are mostly non-conversation (`queue-operation`, `attachment`,
  `mode`, `ai-title`, `file-history-*`) — filter by `type`, follow `parentUuid`.
- The Codex rollout fd is held only while loaded/writing — **fd presence is not a liveness
  signal**; use `~/.codex/state_5.sqlite` or the app-server.
- **Rollout compression is not a live hazard.** `rollout/src/compression.rs` requires
  `ThreadStoreConfig::Local` *and* a default-off feature flag, enforces `MIN_ROLLOUT_AGE = 7
  days` against mtime, and explicitly skips referenced and fork-pointed rollouts; reads are
  transparent and appends re-materialize plain `.jsonl`. rev 1 listed this as a hazard and
  spent a fault-injection slot on it; both reclaimed.

---

## 8. Testing

**E2E through real harnesses is the test.** Only inference is canned.

- **L1 — pure units.** Spec compilation, IR normalization, journal replay, capability
  resolution, ownership, ordering. Most of the code.
- **L2 — fixture replay.** Recorded real streams replayed into planes, asserting on the IR.
  Also format-drift detection. **Subject to §7.1 redaction rules.**
- **L3 — fault injection against real harnesses.** Kill mid-turn, truncate a transcript line,
  block a socket, leave a permission unanswered, attempt a double-open, crash the supervisor
  mid-spawn. Small. (The `.zst` case is dropped per §7.9.)
- **L4 — the E2E layer: real harness, canned model.** Real binaries, pty, transcripts, MCP,
  subagent spawning, driven through marion.
- **L4.5 — self-hosted TUI driver.** marion hosts marion; `TestBackend` + `insta` +
  `assert_scrollback_lines`. DECSET 2026 frame brackets give a precise "assert now" signal. No
  AI in the loop, so it gates commits.
- **L5 — live smoke.** Nightly, real models, structural assertions only.
- **L6 — agent-driven acceptance.** Against the canned provider, so only the tester is
  stochastic. Cross-checks **screen against IR log** (needs `mono_ns`, §4.2) and attaches
  artifacts as evidence.

**`marion doctor`** probes each installed harness and produces the static capability table
(§3.3) — same code as runtime negotiation. Port probe logic from
`registry/.github/workflows/protocol_matrix.py`. It must include a **keystroke-injection
submit check**, since that is the most version-fragile mechanism and currently the least
covered.

**Known limitation:** L1–L4 test marion against harnesses *as recorded*. Drift is caught only
on re-record. This already bit us — Gemini moved 0.40.1 → 0.53.0 before implementation began.
`marion doctor` is the smoke detector, not a guarantee.

---

## 9. Spikes and milestones

rev 1 mixed these: E5's pass criterion was verbatim M2's definition of done, so M0 could not
complete before M2 was built. Separated.

**Spikes (S) — answer a question, emit a fixture, then stop.**

| # | Question | Pass | Fail consequence |
|---|---|---|---|
| **S1** | Claude Code as child from raw Rust: spawn/stream/steer/**interrupt** | interrupt works with no TS in the loop | Claude adapter needs a TS sidecar — **changes the process model** |
| **S2** | Which DECSTBM shape do the harnesses emit, and can the chosen VT keep scrollback? | region top-anchored, or a workable patch identified | scrollback needs custom history above the scroll region |
| **S3** | Codex `shared` lifecycle: does an idle app-server survive? Under which invocation was the ~90s death seen? | reproduced or definitively struck | heartbeat requirement returns |
| **S4** | Can a `Stop` hook actually re-prompt a stopping agent, on Claude Code and Codex? | re-prompt lands and the agent continues | §7.6 needs a different mechanism |

**S1 runs first.** rev 1 put E0 first on self-contradictory grounds — arguing at length that
its risk had dropped and then calling it the sole kill shot. S1's failure changes the language
and process model; S2's changes a crate choice and some scope.

### 9.1 Spike procedures

Each spike is a throwaway binary under `spikes/`, not workspace code. Each writes its fixture
to `tests/fixtures/<spike>/` **after the §7.1 redaction pass**.

**S1 — Claude Code from raw Rust.** Spawn
`claude -p --output-format stream-json --input-format stream-json --include-partial-messages`
with `pty-process`. Feed a user message as one JSON line on stdin; confirm assistant events
stream back. Then, mid-turn, attempt an interrupt on the *input* channel. The framing is
undocumented, so the method is: run the same interaction once under the official TS SDK
(`@anthropic-ai/claude-agent-sdk`) with `strace`/`dtruss` or a stdio tee capturing exactly what
the SDK writes when `interrupt()` is called, then replay those bytes from Rust.
*Assert:* the turn stops within 2s and a subsequent prompt is accepted on the same session.
*Fixture:* the full stdin/stdout JSONL of both runs.
*Also record:* whether `--include-partial-messages` yields token-level deltas.

**S2 — DECSTBM shape.** Run `claude` and `codex` interactively under a pty host that logs raw
output. Grep the captured stream for `\x1b[<top>;<bottom>r` and record every distinct region.
*Assert:* whether any region has `top != 1`. If all regions are bottom-anchored (`top == 1`),
`alacritty_terminal` retains scrollback and M3's scope is as designed. If any has a top offset,
neither candidate crate keeps history and M3 must add a history buffer above the scroll region
— cost that estimate before starting M3.
*Fixture:* the raw byte stream as asciicast v3, replayable against both crates.

**S3 — Codex app-server lifecycle.** Start `codex app-server --listen ws://127.0.0.1:PORT`
three ways — bare, via `codex app-server daemon start`, and as a child of a shell that then
exits — each with zero clients, and watch for ≥300s recording SIGTERM origin via
`sudo dtrace`/`execsnoop` if it fires.
*Assert:* which invocation, if any, dies. *Fixture:* a timing log per invocation.
*Then:* repeat with one live thread to confirm whether an occupied server behaves differently.

**S4 — Stop-hook re-prompt.** Configure a `Stop` hook on Claude Code returning
`{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"..."}}`, and the Codex
equivalent. Have the agent stop without calling `report`.
*Assert:* the agent actually receives the context and produces another turn, rather than the
hook merely being logged. *Fixture:* transcripts of both, with and without the hook.

### 9.2 Milestones and acceptance criteria

Each is a gate with an observable test, not a vibe. A milestone is done when its criteria pass
in CI (or, where marked, by a recorded manual run).

**M1 — one real cross-harness hop.** Includes the canned provider, since §8/L4 depends on it.
- A real `claude` (root, `headless`) calls `mcp__marion__spawn` for a `codex` agent type.
- A real `codex` process starts, edits a file in the repo, and calls `mcp__marion__report`.
- The parent receives the **structured** result as a tool result and its next turn references
  the child's output.
- The whole run is driven by the canned provider — no paid tokens, repeatable.
- `events.jsonl` for both nodes replays into an identical tree.

**M2 — supervisor split.**
- `marion-tui` is killed with SIGKILL mid-run; both agents keep running.
- A new TUI attaches and shows the full tree with complete scrollback.
- Per-agent `seq` is contiguous across the kill; where `src_seq` exists it is gap-free.
- Supervisor restart after SIGKILL replays the journal: `ReapedIdle` nodes stay resumable,
  `Live` nodes become `Orphaned`, and no live process is left untracked.

**M3 — tree UI + embedded terminal.**
- A real `claude` TUI runs inside a marion pane: resize is clean, mouse passes through, plan
  mode and a permission prompt both render correctly over a 10-minute manual session (recorded).
- Scrollback behaves per S2's finding.
- L4.5 snapshot tests pass: click-through selects the right node, the tree renders the right
  shape, detach/reattach restores state.

**M4 — N→1.** A real `codex` root spawns a `claude` child through the same code path, with
`parent_id = None` the only difference. M1's criteria hold with the harnesses swapped.

**M5 — ACP breadth.** At least two ACP agents beyond the day-one set run as children via the
single ACP adapter, with `marion doctor` correctly reporting their differing capabilities and
the UI greying out what they cannot do.

---

## 11. Repo layout

Cargo workspace, edition 2024, MSRV pinned in `rust-toolchain.toml`.

```
marion/
  Cargo.toml                # [workspace]
  rust-toolchain.toml
  crates/
    marion-core/            # IR, launch spec, registry model, journal. No I/O side effects.
    marion-proto/           # client↔supervisor JSON-RPC types, shared by both binaries
    marion-term/            # pty host + VT grid + ratatui rendering adapter
    marion-harness/         # ControlPlane/DisplayPlane traits + per-harness impls
    marion-provider/        # canned provider; later, ModelProxy
    marion-supervisor/      # [[bin]] marion-supervisor  (also `mcp` bridge, `doctor`)
    marion-tui/             # [[bin]] marion
  spikes/                   # throwaway spike binaries, not workspace members
  tests/fixtures/           # recorded streams — REDACTION REQUIRED (§7.1)
  docs/specs/
```

`marion-core` must stay free of process spawning and filesystem side effects so L1 tests are
pure. `marion-harness` depends on `marion-core` and `marion-term`, never the reverse.

Binaries: the user-facing command is **`marion`** (the TUI). `marion-supervisor` is started on
demand and also hosts `marion-supervisor mcp` (the per-child stdio bridge, §5.4) and
`marion-supervisor doctor`. `marion doctor` in the TUI proxies to the latter.

---

## 10. Open questions

1. **S1's outcome** decides pure-Rust vs Rust+TS-sidecar. Everything else is stack-agnostic.
2. **The Codex ~90s app-server death is unexplained.** It did not reproduce (alive at 160s, no
   idle reaper in source). Something killed it once — plausibly it was started via
   `codex app-server daemon` or `remote-control`, or was a child of an exiting shell. S3.
3. **DECSTBM shapes are unmeasured** (S2), and M3's scope depends on the answer.
4. ~~**opencode SSE regression** (#27966)~~ — **RESOLVED 2026-07-31.** Issue closed 2026-05-21,
   fixed in **1.15.5+**; local 1.17.3 is clear. Verified empirically at zero inference cost (a
   local mock OpenAI-compatible provider): `message.updated`, `message.part.updated`, and
   `message.part.delta` all arrive on `/event`. Adapter notes:
   - `/event` and `/api/event` emit **identical event ids in identical order**, differing only in
     envelope — `/event` uses `{id,type,properties}`, `/api/event` uses
     `{id,type,data,location}` plus `version`/`seq` on some types.
   - `/api/event` **suppresses** `server.heartbeat`, `plugin.added`, `reference.updated`. Since
     `/event` heartbeats every ~30 s, **prefer `/event`** — it gives free idle-liveness detection,
     which `/api/event` cannot.
   - `OPENCODE_EXPERIMENTAL_EVENT_SYSTEM=true` unlocks the full `session.next.*` family;
     without it only `agent.switched`/`model.switched` appear.
   - **The v2 prompt path is broken in 1.17.3** — `POST /api/session/{id}/prompt` returns 200 and
     never runs; `/api/session/{id}/wait` returns `ServiceUnavailableError`. Use legacy
     `POST /session/{id}/prompt_async`.
5. ~~**Gemini stamped to 0.40.1**~~ — **RESOLVED 2026-07-31 at 0.53.0.** `GOOGLE_GEMINI_BASE_URL`
   still works (verified by a local logging server receiving
   `POST /v1beta/models/{model}:generateContent`, auth as `x-goog-api-key`); `GEMINI_CLI_HOME`
   still isolates; **the HTTPS-unless-localhost restriction is removed** (zero bundle hits, plain
   HTTP non-localhost accepted). **New requirement:** `GEMINI_API_KEY` alone now yields
   `Invalid auth method selected.` — marion must also write
   `<GEMINI_CLI_HOME>/.gemini/settings.json` = `{"security":{"auth":{"selectedType":"gemini-api-key"}}}`.
   No env-var equivalent exists. Headless still needs `--skip-trust` or
   `GEMINI_CLI_TRUST_WORKSPACE=true`.
6. **Windows** is deferred — `pty-process` is Unix-only and `portable-pty` has no async.
7. **Shim fidelity/cost** is now off the critical path (§5.4) but unmeasured if we ever ship it.
