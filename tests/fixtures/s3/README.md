# Spike S3 — Codex app-server lifecycle

codex-cli 0.145.0, macOS 26.5.1 (arm64), 2026-07-31. Source cross-checked against
openai/codex tag `rust-v0.145.0`.

## Verdict

Idle `codex app-server` processes are **never reaped**, under any invocation tested.
The earlier ~86-90 s SIGTERM report does not reproduce and has no mechanism in the source.
**No process heartbeat is required.**

The real hazard is one level down: an **idle thread** is unloaded at
`THREAD_UNLOADING_DELAY = 1800 s`, and an unmaterialized thread is then irrecoverable.

## Invocations tested (zero clients unless noted)

| Case | Invocation | Result |
|---|---|---|
| A | `codex app-server --listen ws://…`, parent alive | alive 321 s, alive 43 min |
| B | `codex app-server daemon start` (unix://, ppid 1) | alive 321 s, alive 43 min, pid never replaced |
| C | same as A but orphaned (ppid 1, shell exited) | alive 321 s, alive 43 min |
| D | bare ws, one client connected then dropped, 1 thread | alive 322 s, alive 43 min |
| E | bare ws, one client held connected+idle, 1 thread | alive 763 s (past the 600 s client idle timeout) |
| F | orphaned ws, client dropped, 1 thread | alive 703 s, alive 43 min |

Files: `{A..F}-timing.log` (5 s poll), `H-thread-unload-1800s.log`,
`G-thread-survival-*.log`.

## Source constants (rust-v0.145.0)

- `REMOTE_CONTROL_CLIENT_IDLE_TIMEOUT` 600 s — `app-server-transport/src/transport/remote_control/client_tracker.rs:27`.
  Drops a *relay client registration* only (`close_client` → `ConnectionClosed`); never exits the process.
  Applies to the outbound ChatGPT relay transport, not to sockets accepted by a local `--listen ws://`.
  Empirically confirmed by case E surviving 763 s with a connected idle client.
- `shutdown_when_no_connections` is gated to **stdio only** — `app-server/src/lib.rs:676-679`.
  A ws/unix listener does not exit when its last client disconnects (cases D, F).
- `THREAD_UNLOADING_DELAY` 1800 s — `app-server/src/request_processors/thread_lifecycle.rs:4`.
  Unloads a thread with no subscribers. This is the one timer that actually bites.
- Only code that SIGTERMs an app-server: `app-server-daemon` `PidBackend::stop`
  (`backend/pid.rs:520` SIGTERM, `:535` SIGKILL after `STOP_GRACE_PERIOD` 60 s, give up at
  `STOP_TIMEOUT` 70 s). Targets only the pid in `~/.codex/app-server-daemon/app-server.pid`,
  start-time-verified (`process_matches_record`, pid.rs:577-587). An unmanaged server is
  refused with "app server is running but is not managed by codex app-server daemon", not killed.
- Bootstrap updater: `app-server-daemon/src/update_loop.rs:46,50` —
  `INITIAL_UPDATE_DELAY` 300 s, `UPDATE_INTERVAL` 3600 s. Installed **only** by
  `codex app-server daemon bootstrap` or `codex remote-control start`; NOT by `daemon start`.
  Earliest possible fire t+300 s, so it cannot explain an 86-90 s death.
- **Nothing in the tree fires at 86-90 s.** Only ~90 s literals are `GUARDIAN_REVIEW_TIMEOUT`
  and `APP_LIST_LOAD_TIMEOUT`, neither of which signals a process.

## Most likely explanation for the original observation

`codex-app-server-test-client`'s `kill_listeners_on_same_port`
(`app-server-test-client/src/lib.rs:687-740`): `lsof -tiTCP:<port> -sTCP:LISTEN`, then
`kill`, then escalate. It SIGTERMs **anything** listening on the target port, including a
server it did not start, with no delay floor. Opt-in via `serve --kill`.
This matches every symptom: external SIGTERM, arbitrary delay, kills a SIGSTOPped process,
and does not reproduce once the port-conflicting helper is gone.

## Thread survival (the part that matters)

`thread/start` returns a rollout `path`, but **the file is not created** until a turn runs.
Verified: no rollout existed on disk for any test thread.

- Same server, thread live: `thread/read` OK; `thread/resume` FAIL `no rollout found`.
- After server restart: `thread/read` FAIL `thread not loaded`; `thread/resume` FAIL `no rollout found`.
- After 1800 s idle, same live server: `thread/read` FAIL `thread not loaded` (see H log —
  OK at age 1241 s, gone by age 1962 s; read-only probes do not refresh the timer).

So a never-turned thread is lost by process death *and* by 30 min of idleness.

## Natural experiment: package swapped under a live daemon server

Mid-spike, `~/.codex/packages/standalone/current` was swapped
**0.145.0 → 0.146.0** at ~13:45 local, by an out-of-band `install.sh` run from a
concurrent process (not this spike).

Daemon-managed server pid 6553 was started at 13:39:57 from the 0.145.0 binary and was
**still alive and unrestarted at 19:22:53 UTC** (~43 min), with the pid file unchanged
(`{"pid":6553,"processStartTime":"Fri Jul 31 13:39:57 2026"}`), long past the 300 s
`INITIAL_UPDATE_DELAY`. `daemon stop` then reported `managedCodexVersion: 0.146.0`,
confirming the symlink had moved beneath it.

This empirically confirms the discriminator: `codex app-server daemon start` does not
install the update loop, so a version swap does **not** replace a running server.
Only `daemon bootstrap` / `remote-control start` arm the updater.

## What marion must do

1. No process heartbeat. Idle app-servers are not reaped.
2. Never run `codex app-server daemon bootstrap` or `codex remote-control start` — those
   arm an hourly updater that SIGTERMs the managed server on a version change, and
   `daemon stop` does not stop the updater (it lives in `app-server-updater.pid` and will
   restart the server afterwards). There is no config key or env var to disable it; the
   only controls are not bootstrapping, or killing the updater pid.
3. Prefer a bare `codex app-server --listen ws://…` that marion owns. It is outside the
   daemon's pid file and is therefore immune to every kill path in codex-rs.
4. Pick ports deliberately and do not share them with `codex-app-server-test-client --kill`.
5. **Materialize threads that matter.** A thread with no turn has no rollout, and is lost
   to both process death and 1800 s of idleness. If marion must hold a thread open across
   idle gaps, either run a turn to force materialization, or keep a subscriber attached,
   or re-create the thread on demand. A 25 s heartbeat would not have helped — the timer
   is on the thread, not the connection, and read-only calls do not refresh it.
