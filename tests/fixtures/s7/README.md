# S7 — does `killpg` on marion's process group reach `codex exec`'s tool-call children?

Run 2026-08-01 against **codex-cli 0.146.0**, macOS (darwin 25.5.0), a **canned**
OpenAI-Responses provider (`spikes/s7/canned_provider.py`). **No model was called and no API
key was used.** Closes design doc §11 item 18.

## ANSWER: **NO.**

`codex exec` puts each tool-call child in **its own session** (`setsid`, not merely `setpgid`).
A `killpg` on the process group marion created for `codex exec` kills codex, but the tool-call
subprocess and its grandchild survive and reparent to pid 1.

Reproduced twice, identical outcome.

## Evidence

`killpg-report.json` is the machine-readable record (the probe's own `s7-report.json`, committed
here under its fixture name); `exec-spawn-child.stream.jsonl` is the redacted `exec --json`
stream from the same run. Parent (`run_probe.py`) spawned
`codex exec` with `preexec_fn=os.setpgid(0, 0)` — exactly marion's timeout spawn.

`ps -o pid,ppid,pgid` plus `os.getsid(pid)`, **before** the kill:

```
   PID   PPID   PGID    SID  COMMAND
 55091  55056  55091  55049  codex exec --json --skip-git-repo-check -C …/wt …
 55296  55091  55296  55049  <HOME>/.local/bin/codex-code-mode-host
 55386  55091  55386  55386  /bin/sleep 900        <- tool-call child
 55395  55386  55386  55386  /bin/sleep 900        <- its grandchild
```

Read the three columns:

- codex itself is pgid **55091** (marion's group) and sid 55049 — it inherits the harness's
  session, so codex does **not** setsid itself.
- `codex-code-mode-host` (the V8 isolate host) is pgid **55296** — its own process group,
  already outside marion's group, though still in marion's session.
- the tool-call child is pgid **55386** *and* sid **55386**. Same value as its own pid: it is a
  **session leader**. `codex exec` calls `setsid` for the command it runs.

Then `os.killpg(55091, SIGKILL)` — codex's (== marion's) group. **After:**

```
   PID   PPID   PGID    SID  COMMAND
 55386      1  55386  55386  /bin/sleep 900
 55395  55386  55386  55386  /bin/sleep 900
```

`os.kill(pid, 0)` returns success for both — no `ESRCH`. codex is gone; the sleepers are
orphaned to `init` and would run for their full 900 s. **That is the leak.**

## Two cases were driven, and only one leaks

The probe issues a single code-mode `exec` tool call that makes two `exec_command` calls:

- **Case A — command completes, leaving orphaned background sleepers.**
  `spawn.sh` backgrounds a child and a grandchild, then exits. All four pids
  (`toolshell`, `child`, `midshell`, `grandchild`) were **dead 1.63 s in**, before the kill —
  i.e. the instant the command finished. **codex reaps its own command's session on
  completion.** Good hygiene, but it is not what saves marion.
- **Case B — command still running when `exec` yields** (`yield_time_ms: 250`, so
  `exec_command` returns a `session_id` with the process alive). This is the actual shape of a
  runaway tool call at timeout expiry. **Both pids survived the killpg.**

So the leak is specific and unavoidable: it is exactly the case marion's timeout exists for.

## The remedy, measured

Stage 3 of the probe collects every distinct `pgid` among codex's descendants **before** the
kill (`[55091, 55296, 55386]`) and `killpg`s each. Result: `remedy_survivors: []` — everything
dies.

The ordering constraint is the whole point: **the pgid set must be captured before the parent
is killed.** Once codex dies its descendants reparent to pid 1, and ancestry-based discovery
finds nothing. There is no `ps` walk that recovers them afterwards.

## Sandbox

`sandbox_mode = "workspace-write"` (seatbelt) was active — `codex exec`'s own
`x-codex-turn-metadata` reports `sandbox: "seatbelt"` (see S6). It is **not** the cause. The
seatbelt profile governs filesystem and network access, not process-group placement; the
leaking pids are visible in `ps` and killable from outside with an ordinary `SIGKILL`. The
`setsid` is codex's own behaviour.

## Implication for marion's timeout

**`setpgid` at spawn + `killpg` at expiry is NOT sufficient.** It kills `codex exec` and any
child codex leaves in the inherited group (`git fetch` for plugin sync did stay in marion's
group), but every subprocess codex starts for a tool call escapes it.

Timeout enforcement needs, at minimum:

1. before signalling, enumerate the descendants of the child pid and collect their distinct
   pgids (a `ps -e -o pid=,ppid=,pgid=` sweep + ancestry closure is enough);
2. `killpg` marion's own group **and** each collected pgid;
3. do the enumeration *first* — after the parent dies the descendants are unreachable.

A single `killpg` on marion's group leaks runaway tool-call processes. The earlier round-19
measurement (a Rust parent reaping a shell child and its backgrounded grandchild) remains correct
about the *mechanism*; it simply does not describe `codex exec`, which opts out of it.

**What this run does not establish.** The remedy was measured **once, in one probe, on one
machine, at one harness version, over a single tool call**. Not covered: several concurrent
tool-call sessions; a tool-call child that `setsid`s again *after* stage 3's enumeration and
before the signal (a race the sweep does not close); and Claude Code, whose tool-call children
were never probed here. The leak itself, by contrast, was reproduced twice.

## Reproducing

```sh
cd spikes/s7
python3 run_probe.py          # starts its own canned provider on 127.0.0.1:8098
```

`run_probe.py` starts the provider, spawns `codex exec` in a fresh process group, waits for
both pidfiles under `wt/`, snapshots `ps`, kills, re-probes, and finally SIGKILLs anything it
recorded so no sleeper is left behind. It prints and writes `s7-report.json`.

The provider deliberately **blocks turn 2** for `S7_HOLD_SECS` (240 s). Without that hold codex
would finish and exit on its own and the kill would prove nothing.

## Redaction

Home paths → `<HOME>`, UUIDs → `<UUID>`. Pids are real and left intact — they are the evidence.
