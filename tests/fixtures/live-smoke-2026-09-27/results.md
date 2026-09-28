| scenario | delegated | expected child: model, status | reported | verify on branch | landed | wall | tokens root / children | every child (harness@depth: exit) | steer |
|---|---|---|---|---|---|---|---|---|---|
| s1 | y | codex: default model, Ok | y | pass | y | 91s | 28322 / 303053 | codex@1: Ok | - |
| s2 | y | claude: haiku, TimedOut | n | pass | y | 247s | 645225 / 498999 (+1 unrecorded) | claude-code@1: TimedOut, codex@2: Ok, codex@2: Ok, codex@2: Ok, codex@2: Ok, codex@2: Ok, codex@2: Ok | - |
| s3 | y | opencode: default model, Failed | n | none | n | 270s | 60408 / 235797 (+2 unrecorded) | acp@1: Failed, opencode@1: Failed, claude-code@1: Ok, codex@2: Ok, codex@2: Ok | - |
| s3-attempt1 | y | opencode: default model, Failed | n | none | n | 196s | 43940 / 237087 (+1 unrecorded) | opencode@1: Failed, claude-code@1: Ok, codex@2: Ok, codex@2: Ok | - |
| s4 | y | codex: default model, Ok | y | pass | y | 132s | 28473 / 580062 | codex@1: Ok | MessageQueued, MessageDelivered via continuation:gen2 |

Run on 2026-09-27 (CDT) with `scripts/live-smoke.sh` at `live-smoke` 5b34cc1 plus the collector
fixes committed beside these files. Versions: claude-code 2.1.283, codex-cli 0.155.1, opencode
1.18.32. Claude roots ran on haiku (`--model haiku`); codex ran on the operator's configured default,
and so did opencode and every child the prompt gave no model. Each scenario had a 600 s wall clock.
"verify on branch" is `python3 -m unittest -q` plus the scenario's own check, run by the suite on
the branch the expected child landed. It is never the child's claim.

`s3-attempt1` is the first s3 run. The operator's opencode default model is an OpenCode Go model
the account has no subscription for. That is an infrastructure cause, so s3 was rerun once as `s3`
with `MARION_LIVE_SMOKE_OPENCODE_MODEL=google/gemini-3.5-flash`. On the rerun, that Google model
answered "high demand" (an outage), so no live opencode child finished in either run.

## Per scenario

- **s1, claude root to codex child: all green.** Haiku delegated on its first turn and declared a
  verification (`python3 -m unittest -q`). Codex read the file, added `word_count` and 7 tests,
  ran the suite, committed in its worktree and called `report` without being asked. The branch
  passed the suite and the check.
- **s2, codex root to claude child: the work landed, but the child never reported.** The codex
  root spawned `claude` on haiku with `background: true`, `timeout_secs: 180` and a verification.
  The claude child wrote correct code within 20 s. It then had no shell to run the tests, because
  its tools were Read, Write and marion's. So it spawned six codex grandchildren in their own
  worktrees to "run the tests" and later to "commit so they're visible across worktrees". Those
  grandchildren started from the base commit, so none of them saw its edits. It was killed at
  180 s. The root checked out the dead child's branch itself, ran the suite, committed the result
  on a branch it created, and was then relaunched for a second generation just to hear about the
  child's end. The branch passed.
- **s3 and s3-attempt1, claude root to opencode child: blocked on the model.** Both times the
  opencode child failed before doing anything (Auth: subscription required; Outage: high demand),
  and marion classified both correctly. Haiku then retried opencode without a model, and later
  fell back to a claude child that fixed the bug, again using codex grandchildren (shared-cwd
  this time) to run the tests. The fallback branches were not judged by the suite.
- **s4, claude root to codex child, with an operator steer at +13 s: the steer worked, but one
  generation late.** The steer was queued while the child was in its first turn. The child
  finished that turn with `average([])` raising `ValueError`, reported, and exited. marion then
  relaunched it (`continuation:gen2`) with the steer, and the second generation changed it to
  return 0.0 and committed. The branch passed the empty-input check. The contract's narrative is
  still the first generation's ("ValueError handling"), and `result_commits` lists only the first
  commit. The child also wrote `tasks/lessons.md`, because the operator's global AGENTS.md asks
  for it. marion recorded that as a scope violation, and the root never mentioned it.
