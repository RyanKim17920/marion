# S15 — how a detached supervisor detaches, and what one tree-wide signal reaches

Run 2026-08-05 on macOS (darwin 25.5.0, arm64), Python 3.14.6, against **codex-cli
0.146.0** and a **canned** OpenAI-Responses provider (`spikes/s15/canned_provider.py`,
inherited from S7). **No model was called and no API key was used. Cost $0.00.**

Closes design doc **§11 item 25**, and with it the open mechanism in **§5.7**. Bounds
**§7.3.2** disposition (a).

## ANSWER

**Detach with a double `fork` plus `setsid`. And §7.3.2's disposition (a) must stay a
per-node kill: one tree-wide signal reaches the supervisor and nothing else.**

Three things were measured, in this order.

1. **What each mechanism makes the supervisor.** Double-`fork`+`setsid` and the
   single-fork `setsid` both give the supervisor a **process group containing only
   itself**, reparented to pid 1, sharing neither session nor group with its launcher.
   An ordinary spawn with the launcher exiting keeps **the launcher's group and the
   launcher's session**.
2. **What one signal to one group actually reaches.** With a real `codex exec`
   mid-tool-call underneath it, a single `killpg` on the detached supervisor's own group
   killed **the supervisor** and **nothing in the tree** — codex survived, and so did the
   live tool-call child and grandchild. Under the inheriting mechanism it also killed an
   innocent sibling job that merely shared the launcher's group.
3. **Why the double fork, and not the single one.** They are identical on every relation
   item 25 names. The tiebreak was measured, not argued: a supervisor that *leads* its
   session acquires a controlling terminal the moment it opens a tty slave without
   `O_NOCTTY`. The double fork's second fork removes that possibility permanently.

`launchd` is **rejected**, on a measurement: see the last section.

## The identity table (`ctty.json`, and `signal.json` → `identity`)

Supervisor's own `getpid`/`getppid`/`getsid`/`getpgid`, and the same values for the
launcher that started it. `ps`'s `sess=` column is useless on macOS — it prints `0` — so
every session number here comes from `getsid(2)`.

| mechanism | ppid | sid | pgid | session leader | group leader | group members | ctty | shares session w/ launcher | shares group w/ launcher |
|---|---|---|---|---|---|---|---|---|---|
| `setsid_leader` (fork + `setsid`) | 1 | own pid | own pid | **yes** | yes | **itself alone** | none | no | no |
| `setsid_double` (fork, `setsid`, fork) | 1 | middle pid (dead) | middle pid (dead) | no | no | **itself alone** | none | no | no |
| `inherit` (spawn, launcher exits) | 1 | **the launcher's session** | **the launcher's group** | no | no | **itself + the launcher's other jobs** | none here; `ttys029` under a real pty | **yes** | **yes** |
| `launchd` (user LaunchAgent) | 1 | **1** (launchd's own session) | own pid | no | yes | itself alone | none | no | n/a |

Two rows deserve reading twice.

- **`inherit` shares the launcher's *session*.** In the identity run the observer and the
  supervisor came back `shares_session_with_observer: true`. Launched under a real pty
  (`hangup.json`), that same supervisor showed `ttys029` in `ps`'s tty column and
  `supervisor_in_terminal_session: true` — a "detached" supervisor still attached to the
  terminal that started it.
- **`launchd`'s session is 1.** Not its own, not the launcher's: launchd's. It is a group
  leader of a singleton group, which is the property that matters for signalling, and it
  gets there without any code in marion.

## The decisive experiment (`signal.json`)

S7's scenario, re-run from a **detached** supervisor. The detached supervisor plays the
role S7's foreground `run_probe.py` played: it spawns `codex exec` with
`setpgid(0, 0)` — what `marion-supervisor` does today — against the canned provider,
whose second turn blocks 240 s so codex cannot finish on its own. S7's case B
(`runaway.sh`, a short `yield_time_ms`) leaves a tool-call child **and** a grandchild
alive while `exec` has already yielded. That is the shape of a runaway at quit time.

Two experiments over that live tree, each run under **all three** mechanisms — including
the chosen one, so nothing below rests on the two `setsid` variants being interchangeable.

### `tree_wide` — one `killpg` on the supervisor's own process group

The reading §7.3.2 says the document does not license. Issued from an outside observer.

| | supervisor | control in supervisor's group | launcher's sibling job | `codex exec` | tool-call child | tool-call grandchild |
|---|---|---|---|---|---|---|
| `setsid_leader` | **DEAD** | DEAD | — | **ALIVE** | **ALIVE** | **ALIVE** |
| `setsid_double` (chosen) | **DEAD** | DEAD | — | **ALIVE** | **ALIVE** | **ALIVE** |
| `inherit` | **DEAD** | DEAD | **DEAD** | **ALIVE** | **ALIVE** | **ALIVE** |

**The hinge clause, answered: yes, it reaches the supervisor itself — and it reaches
nothing else that matters.** §6.7 says the per-child process group "is also what keeps
`killpg` from signalling the supervisor itself"; that is true, and it is true in the
direction that makes a one-signal quit useless. The child's group is not the supervisor's
group, so a signal aimed at either one cannot reach the other. Killing the tree from a
detached supervisor is still the per-node walk §6.7 already specifies.

The `inherit` row carries the extra finding: the sibling job died. It was never part of
marion's tree — it was another job in the group the launcher happened to be in. Under a
real `marion` launched from a shell, that group is the shell's job.

### `two_step` — §6.7's kill, performed **by** the detached supervisor

`run.rs`'s own algorithm, reimplemented in `procid.py` field-for-field (`descendant_pids`,
`pgids_of`, `signal_targets`), run by the supervisor against its live child tree.

| | supervisor | `codex exec` | tool-call child | tool-call grandchild | control in supervisor's group |
|---|---|---|---|---|---|
| `setsid_leader` | **ALIVE** | ZOMBIE | **DEAD** | **DEAD** | ALIVE |
| `setsid_double` (chosen) | **ALIVE** | ZOMBIE | **DEAD** | **DEAD** | ALIVE |
| `inherit` | **ALIVE** | ZOMBIE | **DEAD** | **DEAD** | ALIVE |

The supervisor survives its own tree kill under all three mechanisms, and the runaway
tool-call processes S7 measured escaping a single `killpg` are gone. `ZOMBIE` is codex
signalled but not yet reaped by the supervisor — see "How a false pass was prevented".

**`signal_targets`'s own-group exclusion cost nothing here.** `dropped_by_own_pgid_filter`
is `[]` in **every** run: the supervisor's own group never appeared among the pgids
collected from codex's tree, under either mechanism. The guard in `run.rs` is therefore
free in the measured case — but it is only free because the child gets its own group, and
that is a property of the spawn, not of the detach.

## How a false pass was prevented

Six guards, because "everything died" is the easiest wrong answer to produce.

1. **Enumerate while alive.** The whole pid set is enumerated before any signal, and every
   pid that is required to be alive must read `ALIVE` then, or the run's verdict is
   `UNMEASURED` rather than a pass. (S7's case-A pids are excluded from that requirement:
   S7 measured that codex reaps its own completed command's session, so they are legitimately
   already dead — and they are recorded as such rather than quietly dropped.)
2. **Three-valued liveness.** `kill(pid, 0) == 0` → `ALIVE`; `ESRCH` → `DEAD`; **any other
   errno, including `EPERM`, → `UNKNOWN`, never `DEAD`.** This is the rule
   `marion-testsupport::alive` documents and that this repo previously got wrong. Zero
   `UNKNOWN` results occurred in any run.
3. **Pid-reuse guard.** Every watched pid's `ps -o lstart=` is recorded before the signal.
   A pid that reads `ALIVE` afterwards with a different `lstart` is reported `UNKNOWN`,
   not as a survivor. This is not theoretical here: the machine's pid counter wrapped
   during the run, and `hangup.json` contains a shim at pid 403 with a five-digit parent.
4. **Zombie detection.** `kill(pid, 0)` returns success for a zombie. The first pass
   reported the signalled `codex exec` as a **survivor** for exactly that reason. `ps`'s
   state column now separates `ZOMBIE` from `ALIVE`, and zombies are excluded from the
   survivor list.
5. **Positive control.** A `/bin/sleep` deliberately placed **inside the target group** (a
   child of the supervisor that was not given its own group). If it survives, the signal
   never landed and every `DEAD` in that run is discarded. It died in both `tree_wide` runs.
6. **Negative control.** A `/bin/sleep` outside every target group. If it dies, the signal
   over-reached and the run is discarded. It survived in all four runs.

The observer is also, deliberately, **not the supervisor's parent** — after detaching,
nothing is — so it can never `waitpid` and every answer above comes from probing.

## The `launchd` question, answered separately (`launchd.json`)

Item 25 says an external launcher restarting the supervisor after §5.7's idle exit "would
fight §5.7's lifetime rules, and that conflict is a reason to reject the option". That is
testable, so it was tested: a user LaunchAgent whose payload records its identity and then
**exits cleanly with status 0**, exactly as §5.7's idle exit does.

| `KeepAlive` | starts in 12 s | restarted after a clean exit |
|---|---|---|
| `true` | **2** (distinct pids) | **yes** |
| `{SuccessfulExit: false}` | 1 | no |

**Verdict: reject `launchd`.** The conflict is real and it is the default: with the plain
`KeepAlive` a supervisor that obeys §5.7 and exits is brought back roughly every ten
seconds (launchd's minimum-runtime throttle), with a fresh pid, no clients, no nodes, and
nothing to do — and §5.7 requires that exit to be *journaled as a decision*, so the journal
would fill with exit records that describe a decision the system immediately overrode.

The escape hatch does work — `SuccessfulExit: false` stops the restart — and rejecting the
option anyway is a judgement on top of the measurement, so it is stated as one. With
restart-on-exit disabled, launchd contributes nothing the double fork does not already
give (the identity table above: singleton group, ppid 1, no ctty) while adding an install
step into the user's launchd domain, a plist whose lifetime policy lives outside marion's
journal, and a second authority over the supervisor's existence. §5.7's start rule is
"on demand, by the first client that dials the socket, idempotent under contention" —
launchd's socket activation might serve that, and it **was not measured**.

One incidental measurement worth keeping: a LaunchAgent whose program lives under
`~/Desktop` is refused by macOS TCC and **produces nothing, silently** — `launchctl print`
shows `state = running` while the payload never executes. The first run of this probe
measured that and nothing else.

## Why the double fork rather than the single `setsid` (`ctty.json`)

The two are indistinguishable on everything §11 item 25 asks about. The tiebreak was
measured by making the supervisor open a pty slave without `O_NOCTTY`:

| mechanism | session leader | tty after the open | acquired a controlling terminal |
|---|---|---|---|
| `setsid_leader` | yes | `ttys029` | **yes** |
| `setsid_double` | no | `??` | no |
| `inherit` | no | `??` | no |

A session leader with no controlling terminal takes ownership of the first tty it opens.
A supervisor that has acquired a controlling terminal is reachable by a hangup on that
terminal, which is precisely the attachment detaching was for. marion opens no ttys today
— there is no pty code in `crates/` — but S11 exists, so the day a harness gets a pty is
foreseeable, and the second fork costs one `fork` once.

The price of the double fork, stated plainly: the supervisor's pgid and sid name a process
that no longer exists. `killpg(supervisor_pid)` is **not** how you signal it; marion must
carry the pgid rather than derive it from the pid it already journals.

## What this does NOT prove

- **It does not measure a detached `marion` supervisor.** There is none — §10 keeps M1's
  supervisor in-process. The subject here is a Python stand-in that performs the one
  behaviour that matters for the question (spawn `codex exec` in a fresh process group,
  then kill by `run.rs`'s algorithm). Whether a Rust supervisor detached by the same
  mechanism measures the same is untested, though nothing in the mechanism is
  language-specific.
- **It measures one signal, not a UI.** §7.3.2 (a) also requires a rendered list and a
  confirmation before anything is signalled; none of that exists or was exercised here.
- **One machine, one OS, one codex version, one tool call.** No Linux; systemd user units
  were not touched, so the `launchd` verdict says nothing about them. No concurrent
  children, no second harness — Claude Code's tool-call children are still unprobed
  (S7 said the same).
- **The `inherit` collateral is a model, not a shell.** The "launcher's sibling job" was a
  `/bin/sleep` the probe put in a synthetic group so the collateral would be *measured*
  instead of asserted. A real interactive shell's job-control behaviour, and what `bash`
  or `zsh` does to its jobs on hangup, were **not** measured.
- **Terminal hangup killed nothing** (`hangup.json`): closing the pty master left the
  supervisor alive under all three mechanisms, because the kernel's `SIGHUP` goes to the
  terminal's *foreground* process group and the supervisor was not in it. What a shell
  does to its background jobs on hangup is a different question and is unmeasured; the
  `inherit` hazard recorded above is the retained controlling terminal and the shared
  session, not a death this run observed.
- **It does not license a one-signal quit.** Quite the opposite: it measures that no such
  signal exists. §7.3.2 (a) stays the per-node operation it already is.
- **Nothing was wired into `marion-supervisor`.** This is a spike; the detach is a later
  change that cites it.

## Files

- `signal.json` — the full probe report: per-mechanism identity, the four codex runs, the
  enumerated pid/pgid sets, the `ps` snapshot taken while everything was alive, before/after
  state per pid, and the controls.
- `ctty.json` — the controlling-terminal tiebreak.
- `hangup.json` — the pty hangup measurement.
- `launchd.json` — the LaunchAgent identity and the two `KeepAlive` configurations, with
  every `launchctl` invocation and its exit status.

## Reproducing

```sh
cd spikes/s15
python3 run_probe.py       # identity + the four codex runs   (~6 min, $0.00)
python3 ctty_probe.py      # the double-fork tiebreak
python3 hangup_probe.py    # pty hangup
python3 launchd_probe.py   # bootstraps com.marion.s15.probe into gui/$UID and boots it out
python3 redact.py          # copies the four reports here with paths masked
```

`launchd_probe.py` boots its agent out in a `finally` and asserts it is gone; it writes its
plist and payload to a temp dir, never to `~/Library/LaunchAgents`.

## Redaction

Home paths → `<HOME>`, the temp directory → `<TMP>`, UUIDs → `<UUID>`. **Pids, pgids and
sids are real and left intact** — the relationships between them are the measurement.
