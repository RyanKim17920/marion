# marion

A meta-harness: run any agent harness, on any model, as a first-class subagent of any other
harness — with one UI over the whole tree.

```
Agent(harness, model, tools, prompt, …) -> handle
handle: observe · steer · interrupt · result
```

**Status (2026-09-05):** **M1, M2 and M4 [done]**, **M3 and M5 [partial]**. Eight terminal
harnesses — `claude`, `codex`, `gemini`, `opencode`, `copilot`, `goose`, `cline`, `qwen` — run as
children and roots through one spawn path, each a declarative `HarnessSpec` row; any ACP agent
runs as a child as `acp:<command>`. The delegation hop runs end to end: `spawn` creates a git
worktree, launches a real harness against the canned provider, the child edits a file and calls
`report` through marion's own stdio MCP bridge, and marion returns a task contract derived from
git. `marion <harness>` runs a harness's real TUI through marion's relay for four lanes, and
`marion resume` relaunches a root lost to a supervisor restart. M3 waits on its recorded 10-minute
manual session; M5 on the tree naming an ACP agent's own identity. Start at the milestone ledger's
"Where it actually stands" block for the evidence behind each line.

## Running it

```sh
marion run <agent-type> --prompt "…"      # a headless root (claude, codex, gemini, opencode,
                                           #   copilot, goose, cline, qwen types; --pane for a TUI)
marion <harness> [its own flags]           # the harness's native TUI as a marion root
                                           #   (claude, codex, opencode, copilot lanes)
marion tree · marion attach <agent-id>     # the fleet, and one node's pane
marion resume <agent-id>                   # relaunch a lost root under its own id
marion-supervisor doctor --capabilities --harness acp --acp-command "<cmd>"   # probe an ACP agent
```

Child agent types include `acp:<command> [args…]` for any ACP agent with no adapter code.

**Running the E2E suites against a harness that auto-updated.** `cargo test --workspace` drives
the real `claude`, `codex`, … binaries, gated by `marion_testsupport::PINNED_HARNESSES`: a version
the table does not admit fails by name, never skips. Since 2026-09-06 the gate also *chooses* the
binary: `.cargo/config.toml` runs every test through `scripts/cargo-runner.sh`, which puts an empty
per-process directory first on `PATH`, and the first `on_path(<harness>)` fills it with a symlink to
the newest admitted release still on disk (claude's `~/.local/share/claude/versions/<ver>`, codex's
`~/.codex/packages/standalone/releases/<ver>-*/bin/codex`, npm harnesses under
`~/.local/state/marion/harness-pins/`), so the suite runs the release it was measured on while the
updater's binary stays where it is. A harness with no admitted release on disk (Homebrew's
`opencode`, `goose`) falls through to `PATH` and the gate says so. Run the suite from the workspace
root, through cargo — a test binary started any other way panics at its first gate rather than
probe `PATH` and call it pinned. When a harness has genuinely moved on, admit it with
`scripts/admit-harness.sh <harness> <version>`: it widens that entry in the table (entry zero, the
pin, never moves), re-runs every suite that drives the harness — sequentially, each bounded by
`MARION_ADMIT_BOUND` seconds — restores the table on any red, and on green writes the dated
observation beside the entry and prints the MILESTONES paragraph and commit message. It never
commits; read the diff, re-run the `spikes/` probes the entry's evidence calls for, then commit.

---

## Read these, in this order

| file | what it is |
|---|---|
| `MILESTONES.md` | Goals, principles, verified harness facts. The *what* and *why*. |
| `docs/specs/2026-07-31-marion-design.md` | Technical design, **rev 3**. The *how*. §12 lists everything that was retracted or corrected. |
| `tests/fixtures/s1..s5/` | Recorded streams from the five spikes. The seed of the L2 regression suite. |
| `info.md` | Raw research notes that preceded the design. Superseded; background only. |

Where the first two disagree: `MILESTONES.md` wins on *what*, the design doc on *how*.
Operational detail is deliberately **not** duplicated between them — that duplication is what
caused them to drift apart once already.

## Ground rules that are easy to get wrong

Each was learned the hard way. Full detail in `MILESTONES.md`.

1. **Control is config-time, not runtime.** marion controls harnesses by owning their launch
   configuration, never by parsing a pty. The pty is a display device.
2. **Never re-open a session; never stop holding a *running* one.** Every harness's `resume` starts
   a new process against a single-writer transcript. "Opening" a running subagent is a view switch,
   never a connection event. (Reaping an *idle* node is the one sanctioned exception.)
3. **Nothing enforces single-writer but marion.** Neither Codex nor Claude Code locks a session;
   concurrent opens diverge silently and unrecoverably.
4. **Agent types are launch specs, not personas** — a record that compiles to argv + env + config.
   The harness is just a field.
5. **Direct-MCP spawn is the primary delegation path**, not the Claude Code Agent-tool shim.
6. **Capabilities are resolved and rendered, never assumed.** Degrade visibly.
7. **The supervisor outlives the UI.**
8. **Delegation must be auditable** — every hop produces a task contract, with acceptance criteria
   authored *before* the run.
9. **A node is not done while its children run, and a status update is not a delivery.** A
   non-terminal child never enters the parent's context. This is the bug that makes a subagent
   waiting on *its* children ping its parent with a non-answer.
10. **Never hand back something that needs a monitor to interpret.** A task handle plus "go poll"
    means the supervisor lost ownership — fix it there, don't add a status endpoint.

## Trust the docs, but check the version

Claims are stamped: Claude Code **2.1.220** · opencode **1.17.3** · Gemini CLI **0.53.0** ·
Codex CLI **0.145.0/0.146.0** (stamped per claim; the local install moved mid-research) ·
Copilot CLI **1.0.83** · goose **1.49.0** · Cline **3.0.61** · Qwen Code **0.23.0** (all
2026-09-05). The versions a test will accept are the set `marion_testsupport::PINNED_HARNESSES`,
which as of 2026-09-05 also admits claude 2.1.222–2.1.226 and 2.1.261 and codex 0.146.1/0.147.0.

These tools auto-update and break things. In one day: Gemini moved thirteen minors, Codex updated
itself when a scripted Enter hit its startup prompt, and Codex had already removed
`wire_api = "chat"` entirely.

**Design doc §12 lists forty claims that were retracted or corrected** (fifteen from the
research itself, twenty-five more from audit rounds 5-19 and spike S6 that read the docs against the fixtures and
against the installed binaries) — several stated
confidently before being disproved, and one area (terminals) that was corrected, over-corrected,
and corrected again. Treat anything marked
**UNVERIFIED**, and everything in design doc §11 (Open questions), as a hypothesis. Re-verify with
`claude --version`, `codex --version`, `gemini --version`, `opencode --version`.

## The spikes — S1–S5 resolved 2026-07-31, S6 resolved 2026-08-01

| spike | answer |
|---|---|
| **S1** Claude Code from raw Rust | Drivable, interrupt included (`control_response` in 0.5 ms). **No TS sidecar.** The channel is bidirectional — that is how permission prompts arrive. *(Proven over pipes; pty re-confirmation and a real `can_use_tool` round-trip both owed in M1.)* |
| **S2** Terminals and scrollback | Claude Code uses the **alt screen** for its whole session (so needs no scrollback). Codex uses the main screen, entering alt only for the `/diff` pager. Real hazard is `CSI 3J` on every Codex resize — marion intercepts it. |
| **S3** Codex app-server lifecycle | Never reaped; the earlier "reaping" was most likely our own tooling. But **unsubscribed threads unload after 30 min.** |
| **S4** Stop-hook re-prompt | Works on both via `{"decision":"block","reason":…}`. **Codex hooks fail silently until trusted.** |
| **S5** Late-join subscription | `thread/resume` **is** subscribe. Mid-turn attach works. Approvals fan out to all subscribers — marion must not race a human. *(The fan-out and first-answer-wins semantics are read from source, **not measured**: no probe exercises an approval. Owed when the codex adapter lands.)* |

**S6 is resolved** (`tests/fixtures/s6/`), and it answered all three questions with a canned
provider and a real stdio MCP server — no model call, no API key. **`codex exec` hosts MCP
servers**, so marion's `report` return path exists and **M1 takes the primary branch**; `exec --json`
emits `file_change` items with absolute paths; and `--output-schema`/`--output-last-message`
delivers the document verbatim, with `strict: true` added by codex.

The finding that changed M1 was incidental: **Codex 0.146.0 runs *code mode*** — no top-level
`tools` field at all, tools reaching the model as JavaScript inside a `custom` `exec` tool, so a
real model writes `await tools.mcp__marion__report({…})`. Both the flat and namespaced spellings
are real, at different layers (design §3.1 item 1). marion's canned scripts can still use a plain
`function_call` with `namespace: "mcp__marion"`, which is proven to execute.

The most consequential finding was incidental: **`CLAUDE_CONFIG_DIR` isolation breaks OAuth**,
because the Keychain entry is keyed to the real config dir. Config isolation and subscription auth
are mutually exclusive for Claude Code children.

## Current milestone status

M1's disposable vertical slice is complete: a real `claude` root calls
`mcp__marion__spawn`, a real `codex` child edits a file in a worktree and reports, and the parent
receives the structured task contract through marion's `report` tool over MCP. M2's supervisor
split and M4's fan-in criteria are also complete. M3 and M5 remain partial; their exact remaining
criteria and evidence live in `MILESTONES.md` and are deliberately not duplicated here.

Three debts fell due in M1, all of them designed on decompilation rather than measurement, and all
three are now paid: the pty re-confirmation of S1's protocol (**S11**, `tests/fixtures/s11/`), a
live `SubagentStop` check (**S10**, `tests/fixtures/s10/`), and a real `can_use_tool` round-trip
(**S9**, `tests/fixtures/s9/`) — the inbound half of Claude Code's control channel, which the whole
permission path depends on. Each was measured against the canned provider at zero cost. What each
one *changed* is recorded in design §11 items 1, 2 and 14; item 14 is only partially closed, since
hook callbacks and `request_user_dialog` remain unmeasured.

## Stack

Rust, seven workspace crates. A hand-rolled `posix_openpt` PTY host in `marion-supervisor::pty` ·
`alacritty_terminal` 0.26 · `ratatui` 0.30 + `insta` 1 · a hand-rolled ACP driver
(`marion-supervisor/src/acp_child.rs`) · the canned model provider is the `marion-provider` crate.
`vt100` 0.16 is a **dev-dependency of `marion-term` alone** — evidence for §11 item 10, never a
component; marion ships one VT and it is alacritty's.
Rationale and rejected alternatives in `MILESTONES.md` ("Chosen tooling", with its note on which
picks are actually linked).

## Install the commit gate — one step, do it on clone

```sh
git config core.hooksPath .githooks
```

That is the whole install. `.githooks/pre-commit` runs **L4.5** (design §8) — the self-hosted TUI
driver: `TestBackend` + `insta` + `assert_scrollback_lines` over the committed
`tests/fixtures/s2` captures, keyed on DECSET 2026 brackets. No processes, no network, no model,
about a third of a second.

It runs L4.5 **by name** and nothing else. §8 is explicit that **L7 must never gate a commit**, and
a blanket `cargo test` in a hook is how that happens by accident the day an L7 target exists. The
hook also reads back its own pass count and refuses to succeed if the target went missing or
shrank, because a gate that no-ops when its subject is absent is worse than no gate.

A snapshot diff means the rendered screen changed. Review it with `cargo insta review` and decide;
never accept blind. `*.snap.new` is gitignored so a failed run cannot leave a rival snapshot in the
tree.

## Before writing any test fixture

Recorded fixtures contain system prompts, repo contents, and anything secret that appeared in tool
output. They require a redaction pass and a pre-commit secret scan. Prefer recording against the
canned provider, never a real one. Design doc §7.1.
