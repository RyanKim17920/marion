# S16 — what a harness exit does to its MCP stdio server, and to that server's grandchild

Run 2026-08-06 against **Claude Code 2.1.222** (`claude --version` → `2.1.222 (Claude Code)`;
the repo pins 2.1.220 and the local install has moved on — every claim here is stamped
2.1.222 and nothing was re-run against 2.1.220), macOS (darwin 25.5.0, arm64), Python
3.14.6, probe `spikes/s16/mcp_probe.py` (`spikes/s16/`). **No model was called, no API key
was used, no paid tokens** — `ANTHROPIC_BASE_URL` points at a canned Anthropic-Messages
provider on `127.0.0.1` (`spikes/s16/canned_provider.py`), `ANTHROPIC_API_KEY` is empty and
`ANTHROPIC_AUTH_TOKEN` is the literal non-credential string `s16-not-a-credential`. Cost
**$0.00**.

The question. marion's MCP bridge (`marion-supervisor mcp`) is a stdio subprocess started
by the **harness**, not by marion, and marion is about to background a child inside that
bridge process. So: when a headless `claude` run ends, what happens to its `--mcp-config`
stdio server, and to a grandchild that server started?

## ANSWER

**The harness does not merely close the server's stdin — it never closes it at all. It
signals: SIGINT, then SIGTERM 100 ms later, then (by elimination) SIGKILL within another
~450 ms, all three aimed at the server's pid and not at its process group. The server is
gone. Its grandchild is untouched, is reparented to pid 1, and runs on.**

And therefore, the answer marion actually needs:

> **A backgrounded child running *inside* the bridge process does NOT survive — the bridge
> process itself is SIGKILLed. A child the bridge has *forked into its own process* does
> survive, unsignalled and reparented to init.**

Three consecutive runs plus one with the harness in its own process group, all four
identical on every discriminating reading.

### 1. Signal, not EOF — and no EOF at all

| | `harness` | `.rep2` | `.rep3` | `harness-own-pgroup` |
| --- | --- | --- | --- | --- |
| `initialize` / `tools/list` answered | yes | yes | yes | yes |
| **`stdin_eof` recorded** | **never** | **never** | **never** | **never** |
| SIGINT → SIGTERM | 100 ms | 100 ms | 101 ms | 100 ms |
| SIGTERM → last heartbeat | 385 ms | 348 ms | 341 ms | 353 ms |
| SIGTERM → last `ps` sample seeing it alive | 272 ms | 285 ms | 338 ms | 283 ms |
| SIGTERM → first `ps` sample not seeing it | 424 ms | 444 ms | 475 ms | 431 ms |
| signals delivered to the **grandchild** | **none** | none | none | none |
| grandchild heartbeats after harness exit | 58 | 59 | 58 | 58 |
| grandchild `ppid` after harness exit | **1** | 1 | 1 | 1 |

The probe answers `initialize` and `tools/list` in every run (`rpc_out` records), so this
is the teardown of a server the harness genuinely connected to, not the teardown of one it
never spoke to. **`stdin_eof` never appears in any harness run** — and the probe demonstrably
*can* record one, because the control does (below). The harness reaches for a signal
instead. There is no EOF-then-signal sequence to observe: there is only the signal.

### 2. There is a grace period, and it is two-staged

`SIGINT` → **100 ms** → `SIGTERM` → the server, which handles both and deliberately keeps
running, is still heartbeating **~350 ms after SIGTERM** — and is gone within ~450 ms of it.
The 100 ms SIGINT→SIGTERM gap reproduced to within 1 ms across four runs, which is the
shape of a fixed timer rather than a race.

**The third signal is read by elimination, not from a log line.** SIGKILL cannot be caught.
So the probe was arranged so that every *other* way of dying would leave a record: a
logging handler on **29 signals** (every signal Python accepts one for on this platform;
`handlers_refused` is empty in all four runs), an `atexit` record for any orderly
interpreter shutdown, an excepthook record for a crash, and a `deadline_exit` record for
its own timeout. In all four harness runs:
`server_voluntary_exit: false`, `server_orderly_shutdown: false`, `server_crashed: false`,
`kept_beating_after_last_logged_signal: true`, `died_without_a_record: true`. It is gone,
it did not choose to go, and nothing catchable took it. SIGSTOP would have left it alive.
**Only SIGKILL is left.**

What this fixture does *not* pin down is the exact SIGKILL instant, nor its order against
the harness's own exit: the outside watcher runs `ps` at 100 ms, and in every run the
server and the harness disappear in the **same 100 ms sample** (`server_death_bracket` and
`harness_death_bracket` are byte-identical in all four reports). Read that as "within
100 ms of each other, unordered", not as "simultaneous".

### 3. The grandchild survives, and it was never aimed at

The grandchild is a plain `subprocess.Popen` from the MCP server — no `setsid`, no
`setpgid`. It therefore sits in the harness's own process group and session, which is what
makes its survival evidence rather than an accident of isolation:

```
harness    pid 71416  ppid 70063  pgid 71416  sid 70047   claude -p …
server     pid 71501  ppid 71416  pgid 71416  sid 70047   python3 …/mcp_probe.py
grandchild pid 71509  ppid 71501  pgid 71416  sid 70047   python3 …/grandchild.py
```
(`harness-own-pgroup.json → family_while_alive`, taken while all three were alive.)

The grandchild carries the same 29 signal handlers, and logged **zero** signals. It
heartbeats 58 more times after the harness exits, with `ppid: 1` throughout, and `ps`
reports it `ALIVE` at the end of the 15 s watch. It is reparented to init and **left
alone** — not reaped, not signalled, not stopped.

**`harness-own-pgroup.json` exists to make that a measurement.** In the first three runs
`claude` shared the *runner's* process group, so it could not have used `killpg` without
killing the runner too — "it did not use killpg" would have been unfalsifiable. The fourth
run puts `claude` in a process group of its own (marion's arrangement: `setpgid` at spawn,
§9 / S7 / S15), so `claude` **is** the group leader and its MCP server and that server's
grandchild are the only other members. `killpg` was fully available to it. The result is
unchanged: server dead, grandchild alive on `ppid 1`, and — the sharper reading — **the
grandchild logged no SIGINT and no SIGTERM either**, so even the first two signals were
pid-targeted at the server alone, not sent to the group.

### The negative control

`control.json` is the same probe and the same grandchild with **no harness at all**. The
runner speaks the same `initialize` / `notifications/initialized` / `tools/list` opening and
then closes the probe's stdin itself.

```
logging_works: true        eof_recorded: true       survives_own_eof: true
server beats: 64           of which after EOF: 60
grandchild beats: 64       grandchild at end of watch: ALIVE
signals to server: []      signals to grandchild: []
```

This is the run that gives the harness runs their meaning. S7's lesson is that a leak check
which cannot see a survivor is worse than none; its dual is that a probe whose logging is
broken records silence, and silence is indistinguishable from "the harness killed it
instantly". The control shows the log **can** record an EOF, **can** record 60 heartbeats
after one, and **can** show a live grandchild — so the harness runs' "no `stdin_eof`, no
heartbeats after SIGKILL" is an observation and not an instrumentation failure.

## What this means for marion

- **An EOF-hold mitigation in the bridge is moot for the bridge process.** Holding stdin
  open, or ignoring EOF, changes nothing: the harness never sends an EOF, and the process
  is SIGKILLed ~550 ms after the first signal regardless of what it does with its fds. A
  bridge that ignores SIGTERM buys itself ~450 ms and then dies anyway.
- **Anything marion backgrounds must be a separate process, not a thread or a task inside
  the bridge.** Everything in the bridge's address space dies with it, uncatchably, so
  there is no shutdown hook, no flush, no final journal write on that path. Whatever the
  bridge must durably record has to be on disk *before* the harness's turn ends.
- **A separate process does survive**, unsignalled, reparented to pid 1 — which is exactly
  the untracked-runaway shape §11 item 18 / S7 exists to prevent. Surviving is the easy
  half; being *findable* afterwards is the half this fixture does not address.

## What is here

| path | what it is |
| --- | --- |
| `control.json` | the negative control: probe + grandchild, no harness |
| `harness.json`, `harness.rep2.json`, `harness.rep3.json` | three real `claude -p` runs |
| `harness-own-pgroup.json` | same, with `claude` as its own process-group leader |

Each report holds `versions`, the `claude` argv, env-override **names** only, the
`mcp_config` document as written, `family_while_alive` (`ps` for harness + server +
grandchild taken while all three are running), `probe_start`, the server's and grandchild's
complete JSONL logs, `watch_transitions` (100 ms `ps`, state changes only), `samples`
(250 ms `ps` for the 15 s after the harness exits), a derived `answers` block, and
`cleanup` recording that the spike killed everything it started.

## Reproducing

```sh
sh spikes/s16/run.sh /tmp/s16 8116 3                 # control + 3 harness reps + own-pgroup
python3 spikes/s16/redact.py /tmp/s16                # -> tests/fixtures/s16/
```

Needs real `claude` on `PATH` (or `S16_CLAUDE=<path>`) and Python 3. Knobs:
`S16_OBSERVE_SECS` (post-exit watch, default 15), `S16_LIFE_SECS` (the probe's own
deadline, default 90 — the probe is deliberately **mortal** so the spike cannot leak a
survivor). A fresh canned provider and a fresh port per run, as in S11: the provider's turn
counter is per-process.

The `claude` argv follows `crates/marion-harness/src/adapter.rs`, minus the marion-specific
tool names: `-p --output-format stream-json --input-format stream-json --verbose
--allowedTools mcp__s16probe__noop --strict-mcp-config --mcp-config <path> --setting-sources
"" --model haiku`. The MCP frames the probe speaks are cribbed from
`crates/marion-supervisor/src/bridge.rs` — newline-delimited JSON, no Content-Length,
`protocolVersion "2024-11-05"`, `capabilities {"tools":{}}`, no reply to `notifications/*`.
`spikes/s16/run_probe.py` imports `spikes/s15/procid.py` whole rather than restating S15's
three-valued liveness and pid-reuse guard.

## Redaction

`spikes/s16/redact.py`, no hand editing. Home paths → `<HOME>`, the per-run scratch dir →
`<SCRATCH>` (with and without macOS's `/private` prefix), the temp dir → `<TMP>`, UUIDs →
`<UUID>`. **Pids, ppids, pgids, sids and the monotonic timestamps are left intact** — the
relationships between them (server's absent ppid, grandchild's ppid becoming 1, the
grandchild sharing the harness's group, the SIGINT/SIGTERM/last-heartbeat ordering) are the
entire measurement. No credential appears: the reports record env variable **names** only,
and the `mcp_config` block contains only paths.

## Still unmeasured

- **One CLI version, one OS, one machine.** 2.1.222 on macOS. The 100 ms SIGINT→SIGTERM gap
  and the ~450 ms to SIGKILL are that build's timers; nothing here says they are contractual.
- **The exact SIGKILL instant and its order against the harness's own exit** — 100 ms `ps`
  resolution puts them in the same sample. A run that wants that ordering needs the server
  wrapped in a process that can `waitpid` it and report `WTERMSIG`, which is a different
  probe shape (and one the harness would also kill).
- **A server that exits promptly on SIGTERM.** This probe deliberately refuses to, in order
  to see what follows. Whether the harness *waits* for a well-behaved server, or kills on
  the same timer regardless, is unmeasured.
- **An interrupted or crashed harness.** Only a clean `exit 0` end-of-run was measured. A
  harness killed mid-turn, or one whose turn errors, may tear down differently — and that is
  the case marion's timeout path actually exercises.
- **Whether the surviving grandchild is findable.** It survives; nothing here measures
  whether marion could still locate, attach to, or reap it afterwards.
- **`--permission-prompt-tool stdio`** is not in this argv, so nothing here says whether an
  open permission round-trip changes the teardown.
