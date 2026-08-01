# Fixture review checklist

Required by the design doc §7.1: *"a redaction pass on recording, fixtures in `tests/fixtures/`
with a `REVIEW.md` checklist, and a pre-commit secret scan blocking any fixture that fails.
**Fixtures recorded against a real provider are never committed without a human read.**"*

Fixtures are the largest self-inflicted leak surface in this repo: they are verbatim recordings of
a real harness running on a real machine, and the harness volunteers far more about its host than
the protocol under test needs. This file is the gate. **A fixture is not committable until a human
has read it and filled in a row of the provenance table below.**

---

## 1. Before you record

- Record against the **canned provider** unless the experiment genuinely requires a real one.
  A canned recording needs a scan; a real-provider recording needs a scan *and* a human read.
- Record in a **scratch directory**, not in a real project tree. Point `HOME`/`CLAUDE_CONFIG_DIR`/
  `CODEX_HOME` at the scratch dir too, so the harness enumerates an empty command/skill/plugin set
  instead of the operator's. (S4 did this; S1 did not, which is why S1 needed surgery.)
- Drive only what the experiment needs. Don't let a real conversation into the transcript.

## 2. Scan for — every file, every time

| Class | Examples seen in this repo |
|---|---|
| Absolute home paths | `/Users/<name>/...`, and **path-encoded** forms like `-Users-<name>-Desktop-...` |
| Usernames | bare `<name>` inside socket paths, rollout paths, project-dir slugs |
| Email addresses | rendered in harness status/auth panels |
| Organization / tenant names | product or workspace names baked into built-in agent descriptions |
| API keys and tokens | `sk-ant-`, `Bearer <...>`, `ghp_`, `xox[baprs]-`, `AKIA…`, PEM blocks. Env var **names** (`ANTHROPIC_API_KEY`, `GROQ_API_KEY`) are fine; values are not |
| Session / thread / turn ids | live `session_id`, `threadId`, `turn_id` — replace with a stable fake |
| PIDs, ports, hostnames | `pid`, `*.local`, listening ports |
| **The operator's environment** | the `initialize` / `system.init` catalogues: `commands`, `skills`, `agents`, `plugins` (with paths), `mcp_servers`, `tools` (`mcp__*` entries name the operator's MCP servers), `memory_paths`, `models` |
| **Hook configuration and hook output** | `hook_started` / `hook_response` payloads carry the operator's SessionStart hooks verbatim, including third-party tool instructions and unrelated project context |
| Unrelated repo content | any file body, diff, or tool result from a project that is not marion |

Fast pass (run from the repo root):

```sh
grep -rIonE 'sk-ant-[A-Za-z0-9_-]+|Bearer [A-Za-z0-9._-]{8,}|ghp_[A-Za-z0-9]+|xox[baprs]-|AKIA[0-9A-Z]{16}|-----BEGIN' tests/fixtures/
grep -rIonE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' tests/fixtures/   # expect only redacted@*.invalid
grep -rIn  "$HOME" tests/fixtures/
grep -rIn  "$(id -un)" tests/fixtures/
```

Binary `.raw.bin` captures must be scanned too — `grep -I` will skip them, so use `grep -a`
or a Python pass.

## 3. Replacement conventions

| Original | Placeholder |
|---|---|
| `$HOME` | `<HOME>` |
| username | `<USER>` |
| organization / product name | `<ORG>` |
| scratch working dir | `<SCRATCH>` |
| session / thread / prompt id | `<UUID>`, or a stable fake such as `00000000-0000-4000-8000-000000000001` |
| pid | `424242` |
| email | `redacted@example.invalid` |
| model id, where the roster itself is the leak | `<MODEL>` |

**Exception — byte-addressed captures (`tests/fixtures/s2/`).** The design doc cites literal byte
offsets into those files (§5.3: `ESC[?1049h` at 67 and 1900; the 0.145.0 alt-screen pair at 38963
and 43372), and the captured panels are column-aligned. In `s2/` a substitution **must be
byte-for-byte the same length**, so the table above does not apply: emails became an equal-length
placeholder sized per site (`redacted@e.invalid` in the Claude panels,
`redacted@examp.invalid` in the Codex `/status` panel — match on `redacted@`, not on a fixed
domain), and the 7-byte username became `example` (7 bytes). See `s2/NOTES.txt`.

> **⚠ Never run a redaction regex over raw terminal bytes unanchored.** This rule was learned by
> breaking it: the original `s2` pass matched an email *inside* a CSI sequence at 9 sites, eating
> the SGR parameters and terminator and silently converting `ESC[38;2;153;153;153m` / `ESC[22m`
> into a **DECSTBM scroll-region command**. Equal length is necessary but not sufficient — a
> substitution must also begin and end outside any escape sequence. Repaired 2026-08-01; see
> `s2/NOTES.txt` for the correction and the two design-doc figures it changed.

## 4. Preserve — what makes the fixture a fixture

Redaction must not touch the evidence the docs rely on. After redacting, re-derive and diff:

- **s1** — interrupt latencies (`interrupt_response_latency_s`, `interrupt_to_result_s`,
  `first_delta_after_s`, `followup_latency_s`), the `control_response` envelope shape,
  `still_queued`, `terminal_reason`, `num_turns`, `subtype`, `is_error`, per-file frame-type
  counts, and the fact that `stdout.jsonl` contains **zero inbound `control_request` frames**.
- **s2** — file length, `ESC[?1049h`/`ESC[?1049l` offsets, `CSI 3J` / `CSI 2J` counts,
  DECSET 2026 bracket counts, the DECSTBM region histogram, and the `.cast` resize sequence.
- **s3** — timing-log line counts and the observed lifetimes.
- **s4** — hook payload fields (`stop_hook_active`, `hook_event_name`, `decision`,
  `hookSpecificOutput`) and the per-mode stream differences.
- **s5** — probe event counts (probe3 `A` = 93, `B` = 87, `attachAtMs` = 2602; probe2
  `turnCount` = 3) and the method histograms.

If any of these shift, **say so loudly in the commit message** — the doc's numbers are now wrong.

## 5. Record provenance

Add a row to the table below. A fixture with no row is not reviewed and must not ship.

---

## Provenance

All five spikes were recorded on the same host, macOS 26.5.1 (arm64), `TERM=xterm-256color`.
Redaction pass applied 2026-07-31.

| Dir | What was recorded | Provider | Recorded | Human read | Redaction applied |
|---|---|---|---|---|---|
| `s1` | Claude Code 2.1.220 headless (`-p --output-format stream-json --input-format stream-json`) interrupt protocol: `initialize`, a long turn, `interrupt`, and a follow-up turn. `stdin.jsonl` / `stdout.jsonl` (with `initialize`) / `stdout_noinit.jsonl` (without) / `summary.json`. | **Real** (`total_cost_usd` 0.093935 on the follow-up turn) | 2026-07-31 | 2026-07-31 | `argv` home path → `<HOME>`; session ids → stable fakes; `pid` → `424242`. `initialize` response: the 145-entry `commands` catalogue, 10-entry `agents` catalogue and 5-entry `models` roster replaced with minimal illustrative stubs. `system.init`: `slash_commands`, `skills`, `agents`, `plugins`, `mcp_servers` stubbed, `mcp__*` entries dropped from `tools`, `memory_paths` → `<HOME>`. All 9 `hook_started`/`hook_response` pairs kept (envelope shape is the evidence) with `output`/`stdout`/`stderr` bodies replaced by `<REDACTED_HOOK_OUTPUT>`. Org name → `<ORG>`. `summary.json` 66 KB → 5.7 KB. |
| `s2` | pty captures of Claude Code 2.1.220 and Codex CLI 0.145.0/0.146.0 booting and running local slash commands (`/help`, `/status`, `/diff`, `/exit`) across resizes. Five captures, each as `.raw.bin` + asciicast v3 `.cast`. | **None** — no model calls; only local slash commands were driven | 2026-07-31 | 2026-07-31 | **Equal-length only** (see §3 exception and `s2/NOTES.txt`): emails → per-site placeholder; the username → `example`. Byte offsets, `CSI 3J`/`2J` counts, DECSET 2026 brackets and probe sets verified unchanged. **The DECSTBM histogram was *not* unchanged and the original claim that it was is retracted** — 9 splices landed inside CSI sequences and forged scroll-region commands; repaired 2026-08-01 and re-derived (§3 warning, `s2/NOTES.txt`). |
| `s3` | `codex app-server` (codex-cli 0.145.0) lifecycle probes A–H: idle survival under six invocations, thread survival across restart, and the 1800 s thread-unload timer. Poll logs only. | **None** — lifecycle/timing probes, no turns run | 2026-07-31 | 2026-07-31 | `G-thread-survival-create.log` rollout path `/Users/<name>/.codex/...` → `<HOME>/.codex/...`. No other host data present; `README.md` references only `~/`-relative paths and upstream source lines. |
| `s4` | Stop / SubagentStop hook behaviour for Claude Code 2.1.220 and Codex, across four hook modes (`none`, `block`, `additionalContext`, `exit2`): hook stdin payloads, resulting stream-json, transcripts, and the hook config that produced them. | **Real** (short scripted turns, "say the word alpha") | 2026-07-31 | 2026-07-31 | Already recorded in an isolated scratch `HOME`, so `<SCRATCH>` / `<UUID>` placeholders were applied at capture time and the enumerated command/skill/agent sets are the built-in defaults, not the operator's. This pass removed the residual username inside path-encoded project slugs (`-Users-<name>-Desktop-...` → `-Users-<USER>-Desktop-...`) and the org name in built-in agent descriptions → `<ORG>`. |
| `s5` | `codex app-server` 0.146.0 (`--listen ws://`) multi-client probes: late join (probe1), per-connection `thread/unsubscribe` isolation (probe2), mid-turn attach with full event capture for both clients (probe3), plus the method-list error dump. | **Real** (probes run turns; probe3 captures live `item/agentMessage/delta`) | 2026-07-31 | 2026-07-31 | Username inside the path-encoded scratch cwd / `runtimeWorkspaceRoots` → `<USER>`. Event counts (93 / 87), `attachAtMs`, and method histograms verified unchanged. Message ids (`msg_…`) and thread ids are per-run server-side identifiers, kept because probe1/2/3 correlate events by them. |

### Known residue, accepted

- Per-event `uuid` fields in `s1` and `s5` are random per-run identifiers with no host meaning;
  they are kept because the stream shape and event correlation depend on them.
- `claude-501` in scratch paths is the default macOS uid, not an identity.
- Env-var **names** (`ANTHROPIC_API_KEY`, `GROQ_API_KEY`, `OPENAI_API_KEY`) appear in `s1` and `s4`
  as `apiKeySource` values and in captured tool text. No values are present.
- `s2` retains `/Users/example/.codex/app-server-control/app-server-control.sock` — a redacted
  path, load-bearing for the Codex `/status` panel width.
- A generic ">= 40 character token" rule fires 199 times across the tree. Every hit was read and
  is benign: server-side `msg_…` item ids and `rollout-<timestamp>-<uuid>.jsonl` filenames in
  `s3`/`s5` (needed to correlate events across clients), the two `trusted_hash` sha256 values in
  `s4/codex/config.toml.hooks-state.snippet` (they hash `s4/codex/stop_hook.sh`, which is
  committed alongside), and the Codex tool name `create_source_repository_write_credential`
  rendered in an `s2` `/help` panel. None are credentials.

### Verification run, 2026-07-31

Scans over all 59 files (binaries included): `sk-ant-`, `Bearer <token>`, `ghp_`/`xox*`/`AKIA`/PEM,
non-`@example.invalid` emails, the literal home path, and the username are all **CLEAN**.

Invariants re-derived before and after the pass are byte-identical except for the length of
`s3/G-thread-survival-create.log` (189 → 181 bytes, the home-path substitution; no offset into that
file is cited anywhere). In particular the design-doc §5.3 offsets **still hold exactly**:
`ESC[?1049h` at **67** and `ESC[?1049l` at **5866** in `claude-2.1.220-boot-exit.raw.bin`,
`ESC[?1049h` at **1900** in `claude-2.1.220-boot-help-status-resize.raw.bin`, and the
`38963` / `43372` alt-screen pair in `codex-cli-0.145.0-boot-status-help-diff-resize.raw.bin`.
