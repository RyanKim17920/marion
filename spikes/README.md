# Spikes — the index

Every row below is a spike marion ran: one question, measured against a real binary on a real
machine, recorded so the answer became evidence rather than memory. The rule the project states in
`MILESTONES.md` is *"**Every spike emits a fixture**, so answers become regression tests instead of
evaporating"* — this file is the map of where each one's evidence actually landed.

**Why the numbering is gapped in both directories.** A spike leaves evidence only where it produced
some: some left only captures (`tests/fixtures/sN/`), some only a driver (`spikes/sN/`), most left
both, and one (S18) left neither — so neither directory holds a contiguous `s1..s30`.

**Sources.** The *question* column is taken from `MILESTONES.md` where it records one, otherwise
from the spike's own `tests/fixtures/sN/README.md` title or its *"Why this was measured"* /
*"The question"* section. Rows for `s1`–`s5`, which have no fixture README, are taken from the
provenance table in `tests/fixtures/REVIEW.md`. Where nothing in the repo records a fact, the cell
says **not recorded** rather than guessing.

---

## The ledger

| # | The question it answered | Harness / version measured | Date | Evidence | Status |
|---|---|---|---|---|---|
| **S1** | Claude Code's headless `stream-json` control protocol — the recorded argv, the interrupt sequence, and the follow-up turn after it | Claude Code 2.1.220 | 2026-07-31 | fixtures only — `tests/fixtures/s1/` | done; re-confirmed over a pty by S11 |
| **S2** | What a real TUI writes to a pty across boot, local slash commands (`/help`, `/status`, `/diff`, `/exit`) and resizes | Claude Code 2.1.220; Codex CLI 0.145.0 and 0.146.0 | 2026-07-31 | fixtures only — `tests/fixtures/s2/` | done |
| **S3** | Codex app-server lifecycle — are idle app-servers reaped, and do threads survive a restart? | codex-cli 0.145.0 | 2026-07-31 | fixtures only — `tests/fixtures/s3/` | done |
| **S4** | `Stop` / `SubagentStop` hook behaviour — the payload, and what `block` / `additionalContext` / `exit 2` do | Claude Code 2.1.220; Codex | 2026-07-31 | fixtures only — `tests/fixtures/s4/` | done |
| **S5** | `codex app-server` multi-client semantics — late join, per-connection `thread/unsubscribe` isolation, mid-turn attach | codex-cli 0.146.0 | 2026-07-31 | fixtures only — `tests/fixtures/s5/` | done |
| **S6** | *"Does `exec` host MCP servers?"* — and the two other `codex exec --json` assumptions | codex-cli 0.146.0 | 2026-08-01 | driver `spikes/s6/` + `tests/fixtures/s6/` | done — **YES on all three** |
| **S7** | *"Does `killpg` on marion's process group reach `codex exec`'s tool-call children?"* | codex-cli 0.146.0 | 2026-08-01 | driver `spikes/s7/` + `tests/fixtures/s7/` | done — **NO** |
| **S8** | *"Does config-dir isolation break auth"* — the Codex half only | Codex (version not recorded) | 2026-08-02 | driver only — `spikes/s8/`, whose report *"carries structural facts only … because the spike touched real OAuth credentials"* | partial |
| **S9** | *"The inbound half of Claude Code's control channel"* — answered only for the `can_use_tool` third | Claude Code 2.1.220 | 2026-08-03 | fixtures only — `tests/fixtures/s9/` | partial |
| **S10** | `SubagentStop`, fired live — the confirmation that had been static-only | Claude Code 2.1.220 | 2026-08-03 | driver `spikes/s10/` + `tests/fixtures/s10/` | done |
| **S11** | S1's interrupt protocol replayed verbatim over a real pty — is it the same protocol? | Claude Code 2.1.220 | 2026-08-03 | driver `spikes/s11/` + `tests/fixtures/s11/` (pipes control committed alongside) | done — protocol unchanged, framing is not |
| **S12** | *"Does `GEMINI_CLI_HOME` isolation break Gemini auth, and what does a Gemini adapter need"* | gemini CLI 0.53.0 | 2026-08-03 | fixtures only — `tests/fixtures/s12/` | partial |
| **S13** | *"What an opencode adapter needs, and why §6.4's opencode bullet describes the wrong path"* | opencode 1.17.3 | 2026-08-03 | fixtures only — `tests/fixtures/s13/` (a report; binary inspection and short probes produced no capture) | partial |
| **S14** | *"The harness-native mapping for `read`, measured on all four harnesses"* | claude 2.1.222, codex-cli 0.146.0, gemini 0.53.0, opencode 1.17.3 | 2026-08-05 | fixtures only — `tests/fixtures/s14/` | not recorded |
| **S15** | *"How a detached supervisor detaches, and what one tree-wide signal reaches"* | codex-cli 0.146.0 | 2026-08-05 | driver `spikes/s15/` + `tests/fixtures/s15/` | not recorded (its README closes design §11 item 25) |
| **S16** | *"What a harness exit does to its MCP stdio server, and to that server's grandchild"* | Claude Code 2.1.222 | 2026-08-06 | driver `spikes/s16/` + `tests/fixtures/s16/` | not recorded |
| **S17** | Concurrent `git worktree add` / `remove` against one repository — is the unguarded case a real hazard? | `git` 2.50.1 (Apple Git-155), APFS | 2026-08-06 | driver `spikes/s17/` + `tests/fixtures/s17/` | not recorded |
| **S18** | **not recorded** — no spike text survives; the only trace is its per-row scrollback figure (~3 kB a row at 140 columns), cited in `crates/marion-tui/src/lib.rs` | not recorded | not recorded | neither a driver nor fixtures | not recorded |
| **S19** | *"Does `setsid()` alone claim the controlling terminal on macOS?"* | Darwin Kernel 25.5.0, xnu-12377.121.6~2, arm64 (a C probe, no harness) | 2026-08-07 | driver `spikes/s19/` + `tests/fixtures/s19/` | not recorded |
| **S20** | *"Can two ACP agents beyond the day-one set actually run here?"* | gemini 0.53.0, opencode 1.17.3, codex 0.147.0 (negative control) | 2026-08-08 | driver `spikes/s20/` + `tests/fixtures/s20/` | not recorded |
| **S21** | Drive a real ACP agent to a marion tool call — does `session/new`'s `mcpServers` reach the agent, and what name does the model type? | opencode 1.17.3 | 2026-08-08 | driver `spikes/s21/` + `tests/fixtures/s21/` | not recorded |
| **S22** | *"The ACP Registry's shims, and the third and fourth spelling of one tool"* — are the shims agents that run here, or a paper listing? | `@agentclientprotocol/claude-agent-acp` 0.66.0 and `codex-acp` 1.1.14, over claude 2.1.220 and codex 0.147.0, node 26.7.0 | 2026-08-08 | fixtures only — `tests/fixtures/s22/` (driven by S21's probe) | not recorded |
| **S23** | Can `opencode acp` run under an isolated config against a canned local provider, at $0.00? | opencode 1.17.3 | 2026-08-08 | driver `spikes/s23/` + `tests/fixtures/s23/` | not recorded |
| **S24** | GitHub Copilot CLI as a marion child, against a canned local provider, at $0.00 | copilot 1.0.83 | 2026-09-05 | fixtures only — `tests/fixtures/s24/` | measured with fixtures |
| **S25** | Qwen Code as a marion child, against a canned local provider, at $0.00 | qwen 0.23.0 | 2026-09-05 | driver `spikes/s25/` + `tests/fixtures/s25/` | measured with fixtures; one reading is fixture-only |
| **S26** | goose as a marion child, against a canned local provider, at $0.00 | goose 1.49.0 | 2026-09-05 | driver `spikes/s26/` + `tests/fixtures/s26/` | measured with fixtures |
| **S27** | Cline CLI as a marion child, against a canned local provider, at $0.00 | cline 3.0.61 (`@cline/core` 0.0.82) | 2026-09-05 | driver `spikes/s27/` + `tests/fixtures/s27/` | measured with fixtures |
| **S28** | *"The installed ACP agents nobody had driven, and the fourth spelling of one tool"* — can a fifth agent marion has no row for be read at all? | copilot 1.0.83, qwen 0.23.0, goose 1.49.0, gemini 0.53.0 | 2026-09-05 | fixtures only — `tests/fixtures/s28/` | measured with fixtures |
| **S29** | *"Where `-C` goes on a `codex exec resume`"* | codex-cli 0.147.0 | 2026-09-06 | fixtures only — `tests/fixtures/s29/` | measured with fixtures |
| **S30** | *"Does gemini's system-settings layer merge `mcpServers` per key, or replace it?"* | Gemini CLI 0.53.0, node 26.8.2 | 2026-09-10 | fixtures only — `tests/fixtures/s30/` | measured; the native-facade `gemini` lane is enabled on it |

---

## Notes on the status column

- **done / partial** are `MILESTONES.md`'s own markers, under its stated convention: *"**[done]**
  every acceptance criterion in design doc §9 is met; **[partial]** some criteria met and the rest
  named."* It marks **S1–S7, S10 and S11 [done]** and **S8, S9, S12 and S13 [partial]**, with the
  standing instruction: *"do not read any of them as closed."*
- **measured with fixtures** is the phrasing `MILESTONES.md` uses for **S24–S29** — *"measured with
  fixtures under `tests/fixtures/s24..s29/`"* — without assigning a done/partial marker. S25 carries
  the one reading in that set with *"no Rust assertion behind it"* (qwen 0.23.0's `write_file`
  refusing a relative path), *"which is why `cross_product` has a qwen row and no qwen column."*
- **not recorded** means exactly that: `MILESTONES.md` assigns no status marker to S14–S23 or S30.
  Several of those fixture READMEs state that they close a named design-doc item, and that is noted
  inline, but a README's own claim is not a ledger marker and is not reported here as one.
- No spike in this ledger is recorded anywhere as **superseded**. S11 is the closest case and is not
  one: it *re-confirmed* S1 over a different transport, finding *"identical 38-kind frame sequence,
  byte-identical interrupt `control_response`, 36 of 36 non-delta frames byte-identical"*, while
  the framing differed — so both rows stand.

## Redaction

Every capture under `tests/fixtures/` passed the gate in `tests/fixtures/REVIEW.md`, which holds the
per-directory provenance table, the accepted-residue list, and the rule that *"**Fixtures recorded
against a real provider are never committed without a human read.**"* Read it before recording a
new one, and add a row when you do.
