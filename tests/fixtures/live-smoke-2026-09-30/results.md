| scenario | delegated | expected child: model, status | reported | verify on branch | landed | wall | tokens root / children | every child (harness@depth: exit) | steer |
|---|---|---|---|---|---|---|---|---|---|
| s1 | y | codex: default model, Ok | y | none | n | 65s | 56102 / 196895 | codex@1: Ok, claude-code@1: Ok | - |
| s2 (--allow-wider-children) | y | claude: claude-haiku-4-5-20251001, Ok | y | pass | y | 88s | 144066 / 181382 | claude-code@1: Ok | - |
| s3 | y | opencode: default model, Failed | n | none | n | 107s | 57023 / 54019 (+1 unrecorded) | opencode@1: Failed, claude-code@1: Ok | - |
| s4 | y | codex: default model, Ok | y | none | n | 64s | 35387 / 229907 | codex@1: Ok | MessageQueued, MessageDelivered via app-server:mid-turn |

Run on 2026-09-30 (CDT) with `scripts/live-smoke.sh` on origin/main 5c068523 plus live-fixes-c's
suite change (s2 passes `--allow-wider-children`, recorded in its `result.json`). Versions:
claude-code 2.1.285, codex-cli 0.155.1, opencode 1.18.33. Claude roots ran on haiku; codex and
opencode on the operator's configured defaults. Each scenario had a 600 s wall clock. "verify on
branch" is `python3 -m unittest -q` plus the scenario's check, run by the suite on the branch the
expected child landed, never the child's claim.

## Per scenario

- **s1 and s4, claude root to codex child: the codex child could run no command (a marion bug,
  fixed on live-fixes-c 4e96dd93).** codex 0.155.1 now runs over app-server. A live node kept the
  operator's approval policy; the operator states none and marion pins the worktree `untrusted`,
  so codex sent `item/commandExecution/requestApproval` for every command, `pwd` included
  (`s1/child1-codex.transcript.json`). marion declines a headless node's approvals, every command
  came back `declined`, and the child reported it could change nothing. In s1 the root then fell
  back to a claude child, whose work the suite does not judge. `codex exec` never asks, which is
  why the 2026-09-27 run did not meet this. The fix sends `approval_policy="never"` to a headless
  live codex node; s39 ran on the fixed build and its codex seat wrote, tested and won.
- **s4's steer landed mid-turn** (`MessageDelivered via app-server:mid-turn`), not a generation
  late as on 2026-09-27; the turn it landed in could run nothing, for the reason above.
- **s2, codex root to claude child, with `--allow-wider-children`: all green.** The claude child
  on haiku wrote `char_frequency` and its tests, ran them with its own shell (no grandchildren,
  where 2026-09-27's child had none and spawned six), and reported. The branch passed.
- **s3, claude root to opencode child: blocked on the model (infrastructure).** The operator's
  opencode default is a Gemini free tier whose quota is 0; marion classified it as UsageLimit, and
  the opencode node reported no usage, so none is recorded for it. Haiku fell back to a claude
  child that fixed the bug; the fallback branch is not judged by the suite.
