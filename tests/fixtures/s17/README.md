# S17 — concurrent `git worktree add` / `remove` against one repository

Probe: `spikes/s17/run.sh <workers> <iterations-each> <repetitions>`.
Machine: darwin 25.5.0, `git version 2.50.1 (Apple Git-155)`, APFS, 2026-08-06.

## The question

`spawn::repo_write_guard` serializes `make_worktree` and `cleanup` **within one marion process**.
Its doc comment justified itself by naming a git failure — *"Unable to create '…/index.lock': File
exists"* — and then said plainly that removing the guard did **not** fail `background_spawn.rs`
over ten runs, so the guard had no failing witness in either direction. Two marion processes, or a
marion and the operator's own `git`, are outside the mutex entirely; whether that is a real hazard
had never been measured.

Each worker loops, with no sleeps and no coordination, over exactly the calls
`spawn::make_worktree` and `spawn::cleanup` make, in their order:

```
git -C <repo> rev-parse HEAD
git -C <repo> worktree add -b <tag>-<i> <path> <head>
git -C <repo> worktree remove --force <path>
git -C <repo> branch -D <tag>-<i>
```

## Result: it fails, and **not** where the doc comment said

`index.lock` was never once the failure. Across every run below, `.git/index.lock` does not appear
in any error. Every observed failure is in git's **`.git/worktrees/` bookkeeping**, in three shapes:

```
fatal: could not create directory of '.git/worktrees/r1w6-167': Invalid argument
fatal: could not create directory of '.git/worktrees/r3w3-71': No such file or directory
fatal: failed to read .git/worktrees/r2w3-24/commondir: Undefined error: 0
fatal: failed to read .git/worktrees/r1w2-182/commondir: No such file or directory
```

**The third shape is the important one, and it is worse than a lost operation.** The name in the
message — `r2w3-24`, `r1w2-182` — is **another worker's** worktree, not the failing caller's.
`worktree add` and `worktree remove` both walk the whole of `.git/worktrees/` (they prune as they
go), so a caller is failed by a *sibling's* half-created or half-removed entry, for an operation
that had nothing to do with it. Nothing serializes that directory: it is not the index, and git
takes no lock over it. `Undefined error: 0` is macOS's `strerror(0)` — git read a torn state and
had no errno to report it with, which is the signature of a directory being observed mid-write
rather than of a refusal.

**It cascades into a leak.** A failed `worktree remove` leaves the worktree registered, so the
`branch -D` that follows is refused in turn:

```
ITER r1w3-182 REMOVE FAILED
fatal: failed to read .git/worktrees/r1w2-182/commondir: No such file or directory
ITER r1w3-182 BRANCH-D FAILED
error: cannot delete branch 'r1w3-182' used by worktree at '…/wt/r1w3-182'
```

So one lost race leaves behind a worktree directory, a `.git/worktrees/` entry and a branch — three
pieces of state marion believes it cleaned up. That is a different and larger failure than the
spawn-that-did-not-happen the doc comment anticipated.

## Rate, by concurrency

| workers | iterations each | repetitions | repetitions with ≥1 failure | total failures |
| ------- | --------------- | ----------- | --------------------------- | -------------- |
| 2       | 600             | 3           | 1 / 3                       | 1              |
| 4       | 300             | 3           | **3 / 3**                   | 8              |
| 6       | 200             | 3           | **3 / 3**                   | 5              |

**Two processes are enough** — the rate is roughly one failure per 1 800 operations, which is why
600 iterations found it once and 150 (an earlier run, not tabulated) found it never. At four
concurrent writers it is reliable within a few hundred operations.

## What this settles, and what it does not

**Settled, measured.**

1. Concurrent `git worktree add`/`remove` against one repository from separate processes **does
   fail**, on this git and this filesystem, at two writers.
2. The failure is `.git/worktrees/` bookkeeping, never `index.lock`. `spawn.rs`'s cited failure
   mode was the wrong one and has been corrected in that doc comment.
3. A lost race can leak a worktree **and** a branch, not merely fail a spawn.
4. `repo_write_guard` **is** load-bearing after all, for the in-process case it covers: four
   concurrent writers — marion's own `max_concurrent_children` — reproduce in every repetition
   within 300 iterations each. `background_spawn.rs` performs four `worktree add`s *once*, which is
   three orders of magnitude short of the exposure needed, and that is why removing the guard did
   not make it red. The mutation surviving was a statement about the test's exposure, not about the
   guard.

**Not settled, and not claimed.**

- Whether git upstream considers this a bug. No upstream issue was searched for; this is a black-box
  observation of one released version.
- Whether Linux/ext4 behaves the same. `Undefined error: 0` is a macOS spelling, and the timing of
  a directory create/rename is filesystem-specific. Measured on darwin only.
- Whether marion in practice reaches the exposure. Two roots each spawning children could, and the
  probe says the threshold is low; but nothing here drove `marion` itself, deliberately — the
  question was about git, so the probe is git.
- **No test asserts this.** A test whose verdict is "a race occurred" is a flake by construction,
  and this repo does not widen timeouts to stabilise one. The finding lives here and in the
  `spawn.rs` doc comment, where a future reader deciding whether to delete the guard will meet it.
