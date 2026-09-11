# docs/

Four kinds of document live here, and the difference matters when you read one:

- **design** — states current intent. Trust it, and fix it when the code disagrees.
- **record** — a dated measurement or snapshot. True on its date, and never updated
  afterwards; re-verify before relying on it.
- **superseded** — kept for provenance only. Nothing in it is a current claim.

Two documents outrank everything here: `MILESTONES.md` at the repository root owns *what* and
*why* (goals, principles, verified harness facts, milestone status), and
`docs/specs/2026-07-31-marion-design.md` owns *how*. Where they disagree, that split decides.

## Index

| Document | Date | Status | What it is |
|---|---|---|---|
| [`specs/2026-07-31-marion-design.md`](specs/2026-07-31-marion-design.md) | 2026-07-31 | design | The design, rev 3 — a clean rewrite of rev 2. 5.8k lines, sectioned; §12 holds every correction and retraction. The operational companion to `MILESTONES.md`. |
| [`superpowers/specs/2026-08-09-native-harness-facade-design.md`](superpowers/specs/2026-08-09-native-harness-facade-design.md) | 2026-08-09 | superseded | First design for `marion <harness> <its own flags>`. Header still reads "review pending"; the runtime design below replaced it. |
| [`superpowers/plans/2026-08-09-native-facade-foundation.md`](superpowers/plans/2026-08-09-native-facade-foundation.md) | 2026-08-09 | superseded | The task-by-task implementation plan for that first slice, landed at `27fe62f`. A worklist, not a contract. |
| [`superpowers/specs/2026-08-10-native-facade-runtime-design.md`](superpowers/specs/2026-08-10-native-facade-runtime-design.md) | 2026-08-10 | design | Per-lane vendor enablement and the byte-transparent PTY runtime. This is the native facade's current design. |
| [`superpowers/specs/2026-08-13-native-relay-synchronous-signals-design.md`](superpowers/specs/2026-08-13-native-relay-synchronous-signals-design.md) | 2026-08-13 | design | v5. How the relay owns `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGTSTP` and reconciles terminal size. |
| [`research/2026-08-09-zed-acp-antigravity.md`](research/2026-08-09-zed-acp-antigravity.md) | 2026-08-09 | record | Zed, the Agent Client Protocol, Gemini CLI and Google Antigravity: public behaviour, distribution and protocol contracts as of that date. Official sources only. Date-sensitive by construction. |
| [`research/2026-07-31-sdk-sessions-and-graph-plans.md`](research/2026-07-31-sdk-sessions-and-graph-plans.md) | 2026-07-31 | superseded | The raw question-and-answer transcript that preceded the design. Background only; it was `info.md` at the root until 2026-09-11. |

`docs/superpowers/` is named for the workflow that produced those documents, not for a
subsystem — there is no "superpowers" component in the code.
