# Live opencode and pi runs, 2026-09-30

Four real delegations on the operator's own logins, run with `scripts/live-smoke.sh` (scenarios
s3, s5, s6, s7) from branch `oc-pi-parity`: opencode as a claude root's child, pi as a claude root's
child, and an opencode root and a pi root each delegating to codex. Each scenario gets a fresh
fixture repository (`textutil.py` and its unittest suite) and is judged by the suite plus a check
run on the branch the child landed (`verify.log`).

Harnesses: claude 2.1.285 (haiku roots), codex 0.159.2 (ChatGPT login, default model),
opencode 1.18.33, pi 0.80.2 (the operator's install; the 0.99.1 admitted the same day runs only
through the test shim). Models: opencode and pi named explicitly with `--model`, because the
operator's defaults are unusable (`opencode-go/*`: "An active OpenCode Go subscription is
required"; pi's default is the same provider).

| scenario | root → child | model | delegated | reported | verify on branch | landed | tokens root / children |
|---|---|---|---|---|---|---|---|
| s3 | claude → opencode | `google/gemini-3.1-flash-lite` | y | y | pass | y | 35 911 / 301 566 |
| s5 | claude → pi | `google/*` (several) | y | n | see below | see below | see `results.md` |
| s6 | opencode → codex | root `google/gemini-3.1-flash-lite`, codex default | y | y | pass | y | 167 395 / 267 207 |
| s7 | pi → codex | root `google/gemini-3.1-flash-lite`, codex default | y | y | pass | y | 30 408 / 354 623 |

**s5 is blocked by the provider, not by marion.** Every model the operator's pi can reach failed
upstream: `opencode-go/*` refused (no active subscription, or "requires Global regions"),
`github-copilot/*` refused (`gpt-5-mini` "not supported"; others not listed by pi 0.80.2), and
Google's free tier answered 503 "high demand" or 429 "exceeded your current quota". A live pi node
keeps the operator's own extensions (by the row's design), and here they put ~66 k tokens into
each request, which exhausts the free tier's per-minute quota fast. The kept attempts:

- `s5-a7-quota/` (`google/gemini-2.5-flash`): the pi child read the file, fixed the off-by-one and
  its branch passed verification and landed; it then spawned two grandchildren of its own (one on
  the operator's lapsed default, one on the retired gemini CLI) and ended on a 429. The scenario's
  prompt now tells the child to do the work itself.
- `s5/` (`google/gemini-3.5-flash`): a 503 on the first request, reported to the root as such.
- `s5-a1-acp-child-hang/`: **marion bug, fixed.** haiku picked `acp-opencode` with a bare
  `deepseek-v4-flash`; opencode's ACP session does not offer that id, marion refused the child after
  its process started, and nothing closed the child's event stream, so the root's blocking `spawn`
  waited out the scenario's 420 s wall.
- `s5-a5-handshake-exit/`: **marion bug, fixed.** A pi child given a model the operator's pi does
  not list wrote `Error: Model "…" not found` to stderr and exited before its `get_state` reply;
  marion told the root only that "the node exited before answering marion's initialize control
  request".

`results.md` is the suite's own table over every run this day (sixteen, including the pruned
attempts on unusable models, which are rows only). Redaction is the collector's
(`scripts/live-smoke-collect.py`: scratch root, home, user, UUIDs, `ses_*`, catalogue keys, rate
limit info) plus seven locally installed tool and skill names replaced by same-length `toolNN…`
placeholders (`scripts/fixture-privacy.py --fix`).

Spend, approximately: claude ≈ $1.2 at list price (haiku roots ≈ $0.5 over sixteen runs; six claude
children the roots started as a fallback ≈ $0.7), codex ≈ 0.6 M tokens on the ChatGPT login (mostly
cache reads), Google API free tier $0, no opencode-go or Copilot usage billed (all refused).
