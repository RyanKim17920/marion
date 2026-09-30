# s39: a workflow on real agents

`scripts/live-smoke.sh s39`: no root; `marion workflow run s39` drives plan (claude haiku,
read-only) → race codex / opencode / pi → review by another model family, up to 3 rounds → land on
a branch, to fix `last_n_lines`' off-by-one. Run 2026-09-30 (CDT) on live-fixes-c 4e96dd93, the
codex approval fix included, with claude-code 2.1.285, codex-cli 0.155.1, opencode 1.18.33 and pi
0.80.2 on the operator's own logins. Wall clock 2400 s.

| run | outcome | steps | race | review | verify on branch | landed | wall | tokens (claude / codex) |
|---|---|---|---|---|---|---|---|---|
| s39 | succeeded | plan succeeded, build succeeded, gate clean, land succeeded | codex won; opencode and pi failed | claude, other family: y, 0 fixes | pass | y | 223 s | 254,904 ($0.28) / 299,492 |
| s39-attempt1 | succeeded | the same | codex won | claude, 0 fixes | pass | y | 151 s | not collected |

`s39-attempt1` is the first run. It succeeded, but the scenario's judge saved no transcripts or
tokens and the suite's cleanup removed its state, so s39 was run once more (an infrastructure
cause, in the test driver; fixed with the judge now sharing the collector). Its `run.out` is kept.
In the second run the judge read a token total the collector computes elsewhere, so each node's
tokens in `s39/result.json` were recomputed from its contract (`tokens_from: contract`).

The race's other two seats failed on the operator's provider accounts, not on marion: opencode's
default model is a Gemini free tier whose quota is 0 (UsageLimit), and pi's default is an OpenCode
Go model the account has no subscription for (marion ended the run as the harness retried a
refused credential). The workflow went on because a race needs one passing seat.
