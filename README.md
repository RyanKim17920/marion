# marion

A meta-harness: run any agent harness, on any model, as a first-class subagent of any other
harness — with one UI over the whole tree.

Everything serves one primitive:

```
Agent(harness, model, tools, prompt, …) -> handle
handle: observe · steer · interrupt · result
```

**Status:** design complete, nothing built. Start at §"First task" below.

---

## Read these, in this order

| file | what it is |
|---|---|
| `MILESTONES.md` | Goals, non-negotiable principles, and **verified facts** about each harness. The *what* and *why*. |
| `docs/specs/2026-07-31-marion-design.md` | The technical design, rev 2. The *how*. |
| `info.md` | Raw research notes that preceded the design. Superseded — read only for background. |

Where `MILESTONES.md` and the design doc disagree: `MILESTONES.md` wins on *what*, the design
doc wins on *how*.

## Ground rules that are easy to get wrong

These are load-bearing and were each learned the hard way. Full detail in `MILESTONES.md`.

1. **Control is config-time, not runtime.** marion controls harnesses by owning their launch
   configuration, never by parsing a pty. The pty is a display device.
2. **Never re-open a session; never stop holding one.** Every harness's `resume` starts a *new
   process* against a single-writer transcript. marion holds each child's channel from spawn.
   "Opening" a running agent is a view switch, never a connection event.
3. **Nothing enforces single-writer but marion.** Neither Codex nor Claude Code locks a
   session; concurrent opens diverge silently and unrecoverably.
4. **Agent types are launch specs, not personas** — a record that *compiles* to argv + env +
   config. The harness is just a field.
5. **Direct-MCP spawn is the primary delegation path**, not the Claude Code Agent-tool shim.
   The shim launders results through an extra LLM turn and costs a full ~462 MB process.
6. **Capabilities are negotiated and rendered**, never assumed. Degrade visibly.
7. **The supervisor outlives the UI.**

## Trust the docs, but check the version

Every harness claim is stamped to a version: Claude Code **2.1.220**, Codex CLI **0.145.0**,
opencode **1.17.3**, Gemini CLI **0.53.0**. These tools auto-update and break things — during
design alone, Gemini moved thirteen minor versions and Codex removed `wire_api = "chat"`
entirely.

**Anything marked UNVERIFIED or listed in the design doc's §10 Open Questions is not
established.** Two prior claims were confidently wrong and had to be retracted (see the
RETRACTED / CORRECTED blocks in `MILESTONES.md`) — treat unverified assertions as hypotheses.

Re-verify before relying on anything version-specific: `claude --version`, `codex --version`,
`gemini --version`, `opencode --version`.

## All five spikes are resolved — start building

S1–S5 ran on 2026-07-31 and **all passed**. Their recorded streams are in `tests/fixtures/`
and are the seed of the L2 regression suite. Full results in `MILESTONES.md` and the design
doc; the one-line version:

| spike | answer |
|---|---|
| **S1** Claude Code from raw Rust | Drivable, interrupt included (`control_response` in 1 ms). **No TS sidecar.** |
| **S2** DECSTBM / scrollback | `alacritty_terminal` retains it. Claude Code uses the **alt screen**, so it needs none. |
| **S3** app-server lifecycle | No reaper exists. But **unsubscribed threads unload after 30 min.** |
| **S4** Stop-hook re-prompt | Works on both, via `{"decision":"block","reason":…}`. |
| **S5** late-join subscription | `thread/resume` **is** subscribe. Mid-turn attach works. |

**Next task: the M1 vertical slice.** A real `claude` root calls `mcp__marion__spawn`, a real
`codex` child edits a file in a worktree and reports back, and the parent receives a structured
task contract — driven entirely by the canned provider so it costs nothing and repeats.
Acceptance criteria are design doc §9.2; workspace layout is §11.

Build it **disposably first**. The independent Codex review argued — persuasively — that the
delegation core should be proven before anything that displays it. No daemon, no VT emulator,
no model proxy, no event log beyond a task audit trail. Only once that hop works should the
supervisor split (M2) and the tree UI (M3) go in.

## What is still open

Small, and none of it blocks M1:

- **`SubagentStop` is verified statically only** — it shares Claude Code's `Stop` code path in
  the 2.1.220 bundle. Owed: a live confirmation under real auth.
- **Does `CODEX_HOME` / `GEMINI_CLI_HOME` isolation break auth** the way `CLAUDE_CONFIG_DIR`
  does? Unverified.
- The Codex **daemon-updater kill path** is source-verified, not measured (running
  `daemon bootstrap` installs durable user state, so the spike declined to).
- An **alacritty edge case** (`grid/mod.rs:258`) unreachable in observed traffic but unproven
  for one huge tool output on a very short terminal.
- **Windows.** `pty-process` is Unix-only.

## Repo layout and milestone gates

Cargo workspace layout is design doc §11; per-milestone acceptance criteria are §9.2. There is
no code yet — M1's gate is a real `claude` root spawning a real `codex` child over the
direct-MCP path, driven entirely by the canned provider so it costs nothing and repeats.

## Stack

Rust. `pty-process` 0.5.3 (`features = ["async"]`) · `alacritty_terminal` 0.26.0 ·
`ratatui` 0.30.2 + `insta` 1.48.0 · `agent-client-protocol` 2.0.0 · `wiremock` 0.6.5.
Rationale and rejected alternatives in `MILESTONES.md` → *Chosen tooling*.

## Security, before writing any test fixtures

Recorded fixtures contain system prompts, repo contents, and anything secret that appeared in
tool output. They must go through a redaction pass and a pre-commit secret scan before being
committed. Prefer recording against the canned provider, never a real one. See design doc
§7.1.
