# marion

A meta-harness: run any agent harness, on any model, as a first-class subagent of any other
harness — with one UI over the whole tree.

```
Agent(harness, model, tools, prompt, …) -> handle
handle: observe · steer · interrupt · result
```

**Status:** design complete, spikes S1–S5 resolved, **S6 open and owed first**, **no code yet.**
Start at *First task*.

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
Codex CLI **0.145.0/0.146.0** (stamped per claim; the local install moved mid-research).

These tools auto-update and break things. In one day: Gemini moved thirteen minors, Codex updated
itself when a scripted Enter hit its startup prompt, and Codex had already removed
`wire_api = "chat"` entirely.

**Design doc §12 lists thirty-three claims that were retracted or corrected** (fifteen from the
research itself, eighteen more from audit rounds 5-16 that read the docs against the fixtures and
against the installed binaries) — several stated
confidently before being disproved, and one area (terminals) that was corrected, over-corrected,
and corrected again. Treat anything marked
**UNVERIFIED**, and everything in design doc §11 (Open questions), as a hypothesis. Re-verify with
`claude --version`, `codex --version`, `gemini --version`, `opencode --version`.

## The spikes — S1–S5 resolved 2026-07-31, S6 open

| spike | answer |
|---|---|
| **S1** Claude Code from raw Rust | Drivable, interrupt included (`control_response` in 0.5 ms). **No TS sidecar.** The channel is bidirectional — that is how permission prompts arrive. *(Proven over pipes; pty re-confirmation and a real `can_use_tool` round-trip both owed in M1.)* |
| **S2** Terminals and scrollback | Claude Code uses the **alt screen** for its whole session (so needs no scrollback). Codex uses the main screen, entering alt only for the `/diff` pager. Real hazard is `CSI 3J` on every Codex resize — marion intercepts it. |
| **S3** Codex app-server lifecycle | Never reaped; the earlier "reaping" was most likely our own tooling. But **unsubscribed threads unload after 30 min.** |
| **S4** Stop-hook re-prompt | Works on both via `{"decision":"block","reason":…}`. **Codex hooks fail silently until trusted.** |
| **S5** Late-join subscription | `thread/resume` **is** subscribe. Mid-turn attach works. Approvals fan out to all subscribers — marion must not race a human. *(The fan-out and first-answer-wins semantics are read from source, **not measured**: no probe exercises an approval. Owed when the codex adapter lands.)* |

**S6 is open, and it is M1's first task.** It was started and killed mid-run. It answers three
questions about `codex exec --json` that decide what M1 builds — does `exec` host MCP servers (if
not, the `mcp__marion__report` return path does not exist and M1 uses the `--output-schema`
fallback); does it emit file locations (if not, marion loses per-tool-call attribution; the scope check is git-derived either way); and does
`--output-schema`/`--output-last-message` actually deliver a document when the final message is
canned — **and it records two Codex encodings the repo lacks** (a Lark-grammar `apply_patch` call
and a `type:"namespace"` MCP call), without which M1's canned Codex script cannot be written.
Design doc §9 specifies every branch, so M1 is not blocked — but run S6 before writing supervisor
code. Full scope: design doc §11 item 12.

The most consequential finding was incidental: **`CLAUDE_CONFIG_DIR` isolation breaks OAuth**,
because the Keychain entry is keyed to the real config dir. Config isolation and subscription auth
are mutually exclusive for Claude Code children.

## First task: the M1 vertical slice

**Run spike S6 first** (above) — it is cheap, it produces the `codex exec` fixture the repo lacks,
and it selects M1's return channel. Then: a real `claude` root calls `mcp__marion__spawn`; a real
`codex` child edits a file in a worktree and reports; the parent receives a **structured task
contract** — driven entirely by the canned provider so it costs nothing and repeats. Acceptance
criteria and the concrete M1 decisions (marion launches the root node itself, blocking `spawn`,
three-way contract field ownership, detective scope enforcement, `codex exec --json` as the child
surface, and the canned-provider wiring for both processes): design doc §9. Repo layout: §10.

**Build it disposably.** The independent Codex review argued persuasively that the delegation core
should be proven before anything that displays it: no *detached* daemon (the registry, the task
audit trail and the control MCP run in-process), no VT emulator, no model proxy. Only once that hop
works do the supervisor split (M2) and the tree UI (M3) go in.

Three debts fall due in M1, all things currently designed on decompilation rather than measurement:
the pty re-confirmation of S1's protocol, a live `SubagentStop` check, and **a real `can_use_tool`
round-trip** — the inbound half of Claude Code's control channel, which the whole permission path
depends on and which no committed fixture exercises.

## Stack

Rust. `pty-process` 0.5.3 (`features = ["async"]`) · `alacritty_terminal` 0.26.0 ·
`ratatui` 0.30.2 + `insta` 1.48.0 · `agent-client-protocol` 2.0.0 · `wiremock` 0.6.5.
Rationale and rejected alternatives in `MILESTONES.md`.

## Before writing any test fixture

Recorded fixtures contain system prompts, repo contents, and anything secret that appeared in tool
output. They require a redaction pass and a pre-commit secret scan. Prefer recording against the
canned provider, never a real one. Design doc §7.1.
