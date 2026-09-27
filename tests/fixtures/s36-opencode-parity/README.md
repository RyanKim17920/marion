# s36 — opencode parity probes (opencode 1.18.32, 2026-09-27)

Measurements taken to bring the three opencode shapes marion drives — `opencode run` (headless),
`opencode acp` (ACP) and `marion opencode` (native TUI) — level with the claude and codex rows.
Every probe ran against a local OpenAI-compatible stub standing in for a provider (no model, no
login, $0), in a throwaway `HOME`/`XDG_*` sandbox with opencode's own config document naming the
stub — the same isolation marion's canned rows compile. Each probe is one transcript and one
verdict: **PASS** (the harness did what marion needs), **FAIL** (it did not, and the row now
compensates or the gap is named), or **UNSUPPORTED**.

Host: macOS 26.5.1 (arm64, Darwin 25.5.0), load average 50–110 from other agents' runs.

## `run-usage.stdout.jsonl` — `step_finish` token accounting

`opencode run --pure --format json --title t -m canned/canned-1 "say hi"`, stdin closed. The stub
answered one text turn and a final usage chunk:
`{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050,
"prompt_tokens_details":{"cached_tokens":300},"completion_tokens_details":{"reasoning_tokens":7}}`.
opencode sent `stream_options: {"include_usage": true}`.

The `step_finish` it printed: `tokens: {total: 1050, input: 700, output: 43, reasoning: 7,
cache: {write: 0, read: 300}}`. So `input` excludes the cache reads, and `output` **excludes the
reasoning tokens** — unlike codex's and claude's counters. **FAIL for marion before this fixture**
(the row read `output` 43 and lost 7 generated tokens); the row now adds `tokens.reasoning` into
output, and the four counters sum to opencode's own `total`
(`token_usage::opencodes_measured_step_finish_counts_reasoning_as_output_and_sums_to_its_own_total`).

`opencode acp` reports the same split on the `session/prompt` response
(`inputTokens` 700, `outputTokens` 43, `thoughtTokens` 7, `cachedReadTokens` 300,
`totalTokens` 1050), while `codex-acp` (s22) counts its thoughts **inside** `outputTokens`. The
ACP usage rule is one protocol-wide rule, so the opencode ACP agent still under-reports its
reasoning; see MILESTONES.

Redaction: the session id is the stable fake `ses_s36fake0000000000000000001` (same length as the
real one). Part and message ids are per-run and kept.
