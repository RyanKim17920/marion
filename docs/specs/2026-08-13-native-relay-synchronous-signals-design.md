# Native Relay Synchronous Signals Design, v5

- Date: 2026-08-13
- Status: Design only; production remains dark and M3 remains partial.
- Scope: Native CLI relay handling of `SIGINT`, `SIGTERM`, `SIGHUP`, and `SIGTSTP` on supported macOS and Linux targets, with periodic terminal-size reconciliation.

## Decision and invariants

The native relay will combine minimal atomic termination handlers with coordinator-owned mask, terminal, and bounded-output transitions. There is no signal worker, channel, mailbox, `sigwait` worker, helper process, `signalfd`, or `kqueue`. Marion installs no handler for `SIGWINCH`, `SIGTSTP`, or `SIGCONT`.

Activation occurs after bootstrap, claim, socket open, and attach `Ready`, but immediately before the first raw-terminal mutation and keyboard-worker spawn. At that instant the Marion client process must contain exactly one thread: the relay coordinator. The supervisor's PTY and lifecycle workers live in a different process. This audited, closed-process invariant is an activation gate; activation fails if future client topology can no longer prove it.

While ownership is active, the coordinator is the only client thread eligible to run Marion's termination handlers. Every subsequently created client thread inherits those signals blocked. No Marion or foreign client code may mutate an owned disposition until exact restoration. These are supported-process invariants, not synchronization claims about arbitrary process-global `sigaction` users.

The design uses direct `libc` types, constants, and functions, never handwritten ABI declarations. Compilation gates match the native relay: macOS or Linux, `x86_64` or `aarch64`, and the required `target_has_atomic` capability. The implementation uses the already locked `libc` dependency.

## Startup and activation seam

`native_relay::run` is reordered into two internal phases without changing the facade or handoff API. `run` retains the `ClientTtyWitness` and the existing claim throughout both phases:

1. `RawPaneSession::open_attached` performs only socket/protocol setup and waits for attach `Ready`. The retained TTY stays at its witnessed baseline and no keyboard worker exists.
2. `RawPaneSession::activate` acquires `SignalOwner`, makes the input and terminal-output open file descriptions nonblocking while preserving their exact flags, enters raw terminal mode, spawns the keyboard worker, installs the coordinator's active mask, and forces geometry reconciliation.

Signals during selector parsing, TTY capture, bootstrap, claim, socket open, or attach therefore retain the caller's original mask and disposition. Default termination cannot strand a modified terminal because the terminal has not yet changed. An acquisition failure aborts before nonblocking mode, raw mode, or worker creation. Protocol input between attach `Ready` and activation remains in the socket; no terminal output is accepted until activation owns its bounded buffer.

### Acquisition under a blocked set

The private `SignalOwner::acquire` runs on the sole coordinator thread:

1. Build `configured_set = {SIGINT, SIGTERM, SIGHUP, SIGTSTP}` with checked `libc` set operations.
2. Block `configured_set` with `pthread_sigmask(SIG_BLOCK, ...)` and retain the returned `old_mask` exactly.
3. Query and retain the exact prior `sigaction` for each member while the set is blocked.
4. If any configured member has a custom disposition (neither `SIG_DFL` nor `SIG_IGN`), refuse activation. Restore `old_mask` before returning the custom-disposition error; no action, termios, OFD flag, owner, or publication state has changed.
5. Derive `active_term_set` from `INT`, `TERM`, and `HUP` members that were originally unblocked and had `SIG_DFL`; mark `TSTP` eligible only when it was originally unblocked and had `SIG_DFL`.
6. Claim one process-global atomic owner, publish a zeroed atomic termination bitset, and install Marion's handler for each `active_term_set` member while all configured signals remain blocked. Eligible `TSTP` keeps its default action.
7. Make both terminal directions nonblocking, enter raw mode, and spawn the keyboard worker while the owned set is blocked, so the worker inherits that mask.
8. Set the coordinator's active mask to `old_mask` plus eligible `TSTP`: owned termination signals become unblocked only on the coordinator, eligible `TSTP` remains blocked, and excluded members retain their original membership.

`SIG_IGN` and originally blocked members are excluded from ownership and left untouched: Marion does not install an action, interpret or consume them, or change their effective mask membership after acquisition. Any custom action, including a custom `SIGTSTP` action, refuses the entire activation rather than attempting handler chaining or replay. The refusal path preserves exact actions, mask, termios, and OFD flags and never enters raw mode or starts keyboard work.

`pthread_sigmask` returns an error number directly; it is not read through `errno`. Each wrapper follows the documented convention for `sigaction`, `sigpending`, `raise`, and signal-set operations.

### Owner and handler shape

`SignalOwner` is private or `pub(crate)` and contains the exact old mask and owned default actions, derived sets, per-action restoration state, global-publication state, restoration/poison state, and `PhantomData<Rc<()>>`, making it `!Send` and `!Sync`. Exclusive global ownership is claimed and released only while the owned set is blocked.

The process-global handler-visible state is limited to an exclusive-owner marker and a lock-free atomic bitset. A termination handler performs only an atomic OR/store of its signal bit. It allocates nothing, takes no lock, touches no terminal or socket state, and calls no non-async-signal-safe function. Publication, reset, action changes, and owner release occur while all active termination signals are blocked.

## Live-loop state machine

The coordinator retains the existing 50 ms wake. It owns a capped terminal-output buffer and performs only a bounded number of nonblocking writes per tick. `EINTR` and `WouldBlock` preserve the unwritten suffix for a later tick; they are retry states, not independent fatal relay errors. Buffer overflow or failure to make progress within the configured tick/deadline budget is a typed relay error and begins passive cleanup.

### Active

On every tick, the coordinator:

1. Reads terminal geometry unconditionally, compares it with the last successfully forwarded value, and sends an update on change or a forced-reconciliation latch.
2. Accepts socket output only up to the buffer cap and spends the bounded write budget flushing it to the nonblocking terminal output OFD.
3. Atomically takes the termination bitset.
4. Calls `sigpending` to inspect owned default signals; it never calls `sigwait`.
5. Chooses a transition. Any termination bit dominates `TSTP`; distinct termination bits use `SIGTERM > SIGINT > SIGHUP`.

The bitset is unordered state, not an event queue. Standard signals may coalesce, so Marion promises neither arrival order nor counts and does not claim the chosen signal arrived first. Fast resize changes never trigger teardown because resize is geometry reconciliation, not signal ownership. A standalone `SIGCONT` is neither owned nor observed.

### Transition cutoff

For either a termination or `TSTP` candidate, the coordinator first blocks `active_term_set`. When that mask transition returns, no Marion handler can remain executing on that coordinator, and the keyboard worker already has the set blocked. The coordinator then rejects new forwarding, cancels the keyboard worker, shuts down or retains the socket according to the transition, joins the worker, and enters passive terminal cleanup.

Passive cleanup uses the same capped buffer, bounded writer, and a fixed deadline to flush already accepted bytes. It then restores exact termios and both input/output OFD flags even if bytes remain or a write failed. An overflow, stall, or write error is composed with later cleanup failures; terminal restoration is never conditional on a successful flush.

Before restoring any action, the coordinator atomically drains termination bits and queries pending `active_term_set` members while they remain blocked. The union of the initial bits, final drain, and pending members is an unordered, coalescing pre-cutoff set. Deterministic priority selects exactly one captured default termination; no atomic arbitration or generation ordering is claimed. Signals generated after that cutoff remain blocked and pending until a restored prior action is exposed.

## Termination policy

`SIGINT`, `SIGTERM`, and `SIGHUP` share one policy; only signal identity differs.

| Signal | Priority | `SIG_DFL` and originally unblocked | `SIG_IGN` or originally blocked | Custom disposition |
| --- | ---: | --- | --- | --- |
| `SIGTERM` | 1 | Own; ordered cleanup, then selected default delivery. | Exclude and leave untouched. | Refuse activation before terminal mutation. |
| `SIGINT` | 2 | Own; ordered cleanup, then selected default delivery. | Exclude and leave untouched. | Refuse activation before terminal mutation. |
| `SIGHUP` | 3 | Own; ordered cleanup, then selected default delivery. | Exclude and leave untouched. | Refuse activation before terminal mutation. |
| `SIGTSTP` | stop | Keep blocked/pending; reveal original default stop. | Exclude and leave untouched. | Refuse activation before terminal mutation. |

Once any member enters the pre-cutoff set, termination dominates every other relay result. Cleanup is attempted in this order:

1. Latch termination and reject keyboard, socket-output, resize, detach, and control forwarding.
2. Cancel the keyboard worker.
3. Call `shutdown(Shutdown::Both)` on the client socket. This requests lease release; it does not observe it.
4. Join the keyboard worker.
5. Drop every client socket handle.
6. Return terminal-byte handling to passive mode.
7. Restore exact captured termios and open-file-description flags.
8. Final-drain handler bits and query pending active termination members to establish the cutoff and select one member by priority.
9. Restore every owned action while all owned signals remain blocked, recording success per action.
10. Only after all action restores succeed, deliver the one selected default termination through the phased mask below.

The supervisor asynchronously observes client disappearance and drops the lease. Marion neither kills nor signals the supervisor node or its PTY/lifecycle workers; the node and PTY survive for ordinary reconnect.

### Selected default delivery

After all owned actions are restored, the coordinator checks whether the selected signal is pending. If the handler consumed the captured instance and none is pending, it calls `raise(signo)` while the signal remains blocked. If one is pending, it does not synthesize a duplicate.

It then installs `delivery_mask = old_mask union (owned_set minus {selected})`, where `owned_set` is `active_term_set` plus eligible `TSTP`. Only the selected managed signal becomes deliverable, under its restored `SIG_DFL`, and it must terminate the Marion client with that signal identity. Unselected pre-cutoff and later signals are not replayed by Marion; once their masks are exposed, they follow their restored prior actions and ordinary OS coalescing.

Return from the delivery-mask call after exposing a selected default termination is a fatal invariant violation. The coordinator immediately reblocks what it can, keeps the terminal passive, closes the relay, attempts exact restoration, poisons ownership if restoration is incomplete, and reports the invariant failure rather than continuing the loop.

If any owned action fails to restore, Marion never calls `raise`, never installs the delivery or old mask, and never releases publication or ownership. It keeps owned signals blocked, the terminal passive, and the relay closed; returns an explicit partial-restoration error with per-action results; and lets `Drop` retry. A failed retry leaves the process-global owner armed and poisoned so another acquisition cannot overwrite uncertain actions. Failures compose in stable order and are never mislabeled as successful signal termination.

## `SIGTSTP` reveal and resume

An old-blocked or explicit-`SIG_IGN` `SIGTSTP` remains entirely outside Marion ownership: Marion does not block it beyond acquisition, observe it, mutate it, or transition for it. A custom `SIGTSTP` disposition refuses activation. An eligible default `SIGTSTP` stays blocked and pending on every Marion client thread; Marion never consumes it and never changes its action.

A tick's pending observation is only a stop candidate. The coordinator blocks active termination handlers, latches forwarding closed, cancels and joins the keyboard worker, performs bounded passive output cleanup, and restores exact termios and input/output OFD flags. It then performs the final termination drain and pending query. A termination observed before this cutoff wins and enters termination cleanup; no atomic winner or unbounded arbitration loop is required.

If termination is absent, the coordinator re-queries pending `SIGTSTP` immediately before reveal. If `SIGCONT` canceled the pending stop before reveal, Marion does not synthesize one and proceeds to reactivation. If still pending, the coordinator installs a selective reveal mask equal to `old_mask` plus `active_term_set`. This exposes only the original pending `SIGTSTP` among owned signals while all Marion termination handlers remain blocked.

Marion does not call `raise(SIGTSTP)` or `killpg`. The original terminal-generated group signal has already reached foreground-group peers. Synthesis would race cancellation and `killpg` would duplicate peer delivery.

The selective mask change stops Marion under the original default action. An external `SIGCONT` resumes it and execution continues from that mask call; Marion has no `SIGCONT` handler, mask ownership, wait, receipt, or replay. Thus `SIGCONT` either cancels the still-pending stop before reveal or resumes the process after the default stop.

Termination generated after the final cutoff stays blocked and pending during the stop. Immediately after resume, the coordinator reblocks eligible `SIGTSTP`, then drains termination bits and pending active termination members before any nonblocking/raw re-entry. A late termination is handled immediately through the termination path. This bounded two-check dominance protocol needs no atomic arbitration. If reblocking fails, the terminal stays passive and restored, no keyboard worker or raw mode is restarted, the relay socket is closed, exact action/mask restoration is attempted, and the mask error is returned with later cleanup errors.

If no termination is present, the coordinator checks socket health. A healthy relay reapplies exact nonblocking input/output flags, enters raw mode, spawns the keyboard worker while the owned set is blocked, restores the coordinator's active mask, and forces geometry reconciliation. An unhealthy relay closes/detaches with the terminal restored and leaves the node available for ordinary reconnect.

Socket and lease survival during stop is best effort. The supervisor can exhaust its 1024-frame queue or 10-second backpressure allowance while Marion is stopped. Resume therefore supports either continued use of a healthy socket or clean detach and fresh reconnect; it does not promise uninterrupted lease retention.

## Restoration and failures

Explicit restoration blocks `owned_set`, restores every exact prior action with per-action tracking, resets global publication, releases exclusive ownership, and installs exact `old_mask`. It records success only after every required stage succeeds. A delivery mask or temporary stop-reveal mask is not exact restoration. Publication and ownership are not released unless every action restore succeeded.

`Drop` is a last-resort same-thread RAII guard. If explicit restoration is incomplete, it retries blocking and each unrestored action. Only complete action restoration permits publication reset, owner release, and exact old-mask restoration. Otherwise it leaves ownership armed and poisoned and never unblocks uncertain signals. It never records or reports success. Owner scope ends only after keyboard, socket/session, bounded passive output, and exact termios/input-output-OFD cleanup.

Failure injection targets real seams: signal-set construction and membership, `pthread_sigmask`, `sigaction`, `sigpending`, `raise`, atomic owner publication, socket shutdown/health/drop, bounded output enqueue/write/deadline, keyboard cancel/join/spawn, passive/native terminal transitions, exact terminal/OFD restoration, and geometry forwarding. Direct-return-code and `errno` APIs are tested according to their actual contracts.

Error composition preserves the primary failure and appends cleanup failures deterministically. It never claims a lease release that was merely requested, exact restoration that failed, a blocked state that was not proven, or successful replay when a delivery step returned an error.

## Minimal code boundaries

- `src/native_signal.rs`: flat, crate-private direct-`libc` wrappers, `SignalOwner`, atomic handler publication, mask/action snapshots, pending inspection, selected default delivery, and exact restoration.
- Existing `src/native_relay.rs`: retained witness/claim, `open_attached`/`activate`, capped output and bounded writes, coordinator transitions, keyboard/socket ordering, health checks, and forced/periodic geometry reconciliation.
- Existing `src/native_tty.rs`: exact baseline/raw/passive termios and input/output OFD transitions, without signal policy.

There is no new public facade or handoff API, invented `src/native/` hierarchy, runtime daemon, helper, signal worker, or portable abstraction beyond the native relay's current target gates.

## TDD and evidence plan

Every behavior begins with a test that fails for the intended missing production behavior. Compilation failures, fixture bugs, timeouts, and unsupported-platform skips are not RED evidence.

### Unit and wrapper tests

1. Derive owned termination and eligible `TSTP` sets across old-blocked, default, custom, and explicit-ignore actions; custom always produces pre-mutation activation refusal.
2. Prove deterministic `TERM > INT > HUP` selection without count or first-arrival claims.
3. Cover active-mask, delivery-mask, reveal-mask, and exact-old-mask construction member by member.
4. Reduce active, termination, stop-quiesced, delivery, resume, unhealthy-detach, partial-action-restore, and failed-reblock states.
5. Preserve primary errors and stable cleanup-error ordering.
6. Prove atomic publication is exclusive, reset only while blocked, and handler actions are atomic-bit updates only.
7. Compare portable mask membership and relevant `sigaction` fields, never raw struct bytes.
8. Bound output capacity and per-tick/passive-cleanup work; preserve unwritten suffixes across `EINTR`/`WouldBlock` and classify overflow/stall errors.

### Isolated process tests

Use sacrificial subprocesses for default actions. For `INT`, `TERM`, `HUP`, and `TSTP`, cover exact default/explicit-ignore/old-blocked behavior and `WIFSIGNALED` identity for selected default delivery. A causal custom-disposition test proves refusal occurs before nonblocking/raw/keyboard work and leaves masks, actions, termios, and both OFD flag sets byte-for-byte/field-for-field equivalent. Distinct multi-signal cases prove the pre-cutoff set, one deterministic selection, and default termination; same-number bursts assert only permitted coalescing.

Barrier-controlled cases generate terms before and after the final cutoff, including a pending selected member that must not be duplicated and a handler-consumed selected member that requires `raise`. Assertions prove terminal/socket cleanup and complete action restoration precede delivery. Partial action restoration proves no replay or unblocking, publication/ownership remain armed, and `Drop` retry/poison behavior is bounded. Tests inject each real wrapper failure with bounded completion and no stuck child.

### PTY and production-path tests

Use a real, non-orphaned controlling PTY and the production native entry:

- Prove bootstrap and attach `Ready` occur with baseline termios/OFD before acquisition, raw mode, or keyboard spawn. Signals during bootstrap/attach retain original semantics.
- Prove `run` retains the witness and claim, `open_attached` performs only socket/protocol/`Ready`, and `activate` owns signals, both nonblocking OFDs, raw mode, and keyboard creation without a facade or handoff change.
- Change geometry without `SIGWINCH`; prove the 50 ms tick forwards it and activation/resume force reconciliation.
- Flow-control terminal output until writes return `WouldBlock`; prove bounded memory/work, retry after progress, typed overflow/stall, bounded passive failure, and exact termios/input-output-OFD restoration even when bytes cannot flush.
- For each termination signal, prove forwarding closes, keyboard/socket/terminal cleanup and all action restores precede selected delivery, default exit preserves signal identity, lease release is observed separately, and the supervisor node/PTY survives.
- Deliver terminal-generated `SIGTSTP` to a foreground group, observe actual stopped state with bounded `waitpid`, prove peers receive only the original group signal, then send `SIGCONT` and observe resume without a CONT-receipt claim.
- Race rapid `TSTP`/`CONT` to cover cancellation before reveal and resume after stop without synthetic stop. Place termination before the cutoff and after it while stopped to prove pre-cutoff dominance and immediate post-`CONT` handling before raw re-entry.
- Cover custom-refusal, default, explicit-ignore, and old-blocked policies; healthy resume and raw/keyboard restart; forced resize; failed reblock; expired/backpressured socket detach; node survival; and ordinary reconnect.

Every process test uses barriers rather than timing-only sleeps, has bounded waits, reaps descendants, reports session/process-group/foreground topology on failure, and checks exact final terminal/OFD state, masks/actions, descriptor leaks, and stuck leases.

Real runtime coverage is required on macOS and Linux `x86_64`/`aarch64` targets admitted by the implementation gates. Activation remains dark until formatting, strict Clippy, full workspace tests, repeated signal stress, and production-path PTY evidence pass on supported jobs. A required runner that loses capability fails rather than silently skipping.

## Rollout and current state

This design does not promise signal counts, arrival order, standalone `SIGCONT` observation, signal-driven resize, or guaranteed socket/lease survival across a long stop. It does not alter supervisor signal policy, child-process dispositions, or unrelated shutdown behavior.

The committed production-dark async-handler guard foundation exists. Its later termination/redelivery expansion was uncommitted, is rejected, and carries no design authority. Implementing v5 replaces and narrows the committed foundation around default-only ownership, one selected default termination, pending-`TSTP` reveal, bounded output, and fail-safe restoration. Enablement is a separate explicit change after every gate passes; rollback disables activation without removing the tested implementation. Until then there is no runtime claim and M3 remains partial.
