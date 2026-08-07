# S19 — does `setsid()` alone claim the controlling terminal on macOS?

Run: `cc -O0 -o /tmp/ctty_probe spikes/s19/ctty_probe.c && /tmp/ctty_probe`
(the binary is not committed)

## Why this was measured

`crates/marion-supervisor/src/pty.rs` and design §5.3 both carried a claim that turned
out to be **false**, and the claim was load-bearing in the worst way: it was used to
argue that deleting the `TIOCSCTTY` ioctl was an *unobservable* change on macOS
whenever the slave is on fd 0, and therefore that the "drop `TIOCSCTTY`" mutation had
no possible witness in that topology. A mutation believed unkillable is a mutation
nobody tries.

The claim as written:

> Measured on Darwin 25.5.0 with a three-way probe: with the slave on fd 0, `setsid()`
> **alone** already makes the pty the child's controlling terminal and the ioctl is
> redundant.

## What the probe does

Six cells — {slave on fd 0, pipe on fd 0 with the slave on fd 1} x {no `setsid`,
`setsid` only, `setsid` + `ioctl(<the slave fd>, TIOCSCTTY, 0)`}. The parent asks
`tcgetsid(master)`, which is answerable **only** if the pty is some session's
controlling terminal — the same question `pty/tests.rs` asks.

## Measured — Darwin Kernel Version 25.5.0, xnu-12377.121.6~2, arm64 (2026-08-07)

| cell | `tcgetsid(master)` |
|---|---|
| `slave-fd0 / bare` | `-1` ENOTTY |
| `slave-fd0 / setsid` | `-1` ENOTTY |
| `slave-fd0 / setsid+ioctl` | **the child's pid** |
| `pipe-fd0 / bare` | `-1` ENOTTY |
| `pipe-fd0 / setsid` | `-1` ENOTTY |
| `pipe-fd0 / setsid+ioctl` | **the child's pid** |
| `PARENT-opens fd0 / setsid` | `-1` ENOTTY |
| `PARENT-opens fd0 / setsid+ioctl` | **the child's pid** |
| `PARENT-opens pipe / setsid` | `-1` ENOTTY |

The last three cells move the `open()` of the slave from the child to the parent, as
marion does it, and were added after the first run because the first run did not yet
explain the contradiction below. They agree with the six above.

## The contradiction, and what it turned out to be

The probe said `setsid()` alone claims nothing. But deleting the ioctl from
`spawn_pty`'s `pre_exec` left `pty/tests.rs`'s
`the_child_is_a_session_leader_with_the_master_as_its_controlling_terminal` **green**, and
that test asserts exactly `tcgetsid(master) == pid`. One of the two had to be wrong.

Neither was. The test's child is `/bin/sh -c "tty; sleep 5"`, and **macOS's `sh` claims
the controlling terminal itself** when it starts as a session leader without one and its
stdin is a tty. Swapping that child for `/bin/sleep` execed directly, with the ioctl still
deleted, turns `tcgetsid` from the child's pid to `-1`/`ENOTTY` immediately. The shell was
satisfying the assertion.

That is also what produced the false claim in the first place: the earlier three-way probe
this one replaces ran through a shell, so its `setsid`-only cell measured `sh`, not
`setsid`.

## Conclusion

`setsid()` alone claims nothing, in **any** of the nine cells. The controlling terminal is
claimed by the explicit `TIOCSCTTY` and by nothing else. Three consequences for the repo:

1. The shipped code is **correct** — it does issue the ioctl — but the rationale for
   keeping it ("Linux requires it, macOS does not") was wrong; macOS requires it too.
2. Every pty test in `pty/tests.rs` drives its child through `sh`, so every one of them
   inherited the confound. The deletion mutation was **not** observable anywhere in the
   suite, which is the opposite of what the docs claimed.
3. `tiocsctty_and_not_setsid_is_what_claims_the_terminal` is the test that closes it. Its
   child is `/bin/sleep`, execed directly, and the absence of a shell from that topology
   is the whole point of the test rather than an incidental detail.
