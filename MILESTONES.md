# marion — Goals & Milestones

A meta-harness: run any agent harness, on any model, as a first-class subagent of any other
harness — with one UI over the whole tree.

> **Scope of this file:** goals, principles, and verified harness facts. **Operational detail lives
> in `docs/specs/2026-07-31-marion-design.md` (rev 3) and is not duplicated here** — duplication is
> what caused this document to drift out of sync with the design once already. Where the two
> disagree, this file wins on *what* and *why*; the design doc wins on *how*.

## The core contract

```
Agent(harness, model, tools, prompt, …) -> handle
handle: observe · steer · interrupt · result
```

If a milestone doesn't make that primitive better, it isn't a milestone.

## Non-negotiable principles

1. **Agent types are launch specs, not personas.** A declarative record (`harness`, `model`,
   `effort`, `tools`, `isolation`, `prompt`) that *compiles* to argv + env + config + MCP
   injection. That compile step is the per-harness adapter contract.
2. **Control is config-time, not runtime.** marion controls harnesses by owning their launch
   configuration, never by parsing a pty.
3. **The pty is a display device.** Never a control plane.
4. **ACP is a floor, not a ceiling.** Prefer a harness's native control plane; ACP is the breadth
   tier.
5. **Capabilities are resolved and rendered, never assumed.** Two-stage: a static table keyed
   `(harness, version, surfaces)` from `marion doctor`, narrowed by handshake where one exists.
   Terminal-driven surfaces have no handshake and use the static table only. Degrade visibly.
6. **Vendor payloads are carried, never discarded.**
7. **The supervisor outlives the UI.** Killing the TUI must never kill running agents.
8. **Never re-open a session; never stop holding a *running* one.** Every harness's `resume` starts
   a new process against a single-writer transcript. marion holds each running child's channel from
   spawn. "Opening" a running subagent is a view switch, never a connection event.
   *Deliberately reaping an **idle** node is the one sanctioned exception: the process dies, the
   ownership claim is retained, the node stays resumable.*
9. **Direct-MCP spawn is the primary delegation path.** The parent calls `mcp__marion__spawn` and
   receives a structured task contract. The Claude Code Agent-tool shim is optional sugar — it
   launders results through an extra LLM turn and costs a full ~462 MB process per child.
10. **Delegation must be auditable.** Every hop produces a task contract: ownership, base commit,
    acceptance criteria authored *before* the run, declared writable scope checked against observed
    writes, verification evidence, and the resulting diff. Prose from a foreign agent is not a
    result.
11. **A node is not done while its children are running, and a status update is not a delivery.**
    Completion is explicit *and* descendant-gated: an agent may deliberately report early (marked
    as such, with the still-live children listed), but it may not exit without choosing. Crucially,
    a non-terminal child **never enters the parent's context** — status flows to the tree/UI only.
    The harm this prevents is real and observed: in Claude Code a subagent waiting on its own
    children stops its turn, a completion notification fires, and the parent reasons over a status
    message as though it were a result. **marion can gate this correctly where a single harness
    cannot**, because it owns the whole tree including children running in other harnesses.
12. **Never hand back something that needs a monitor to interpret.** Integrations without lifecycle
    ownership return a task handle and polling instructions — each reinventing a task-id format, a
    status command, a cadence, and a caller obliged to poll. When that caller is a model, the
    monitor is unreliable by construction: a handle in the result slot is indistinguishable from an
    answer. Because marion's supervisor owns each child end-to-end, `spawn` can block, `wait` is a
    real primitive, and the tree is already live. **If marion ever returns a handle plus "go check
    status", the supervisor has lost ownership of something — fix it there.**

## Topology

Star, not mesh: 1→N fan-out and N→1 fan-in. Every edge has a parent. A node may address only its
own descendants and its parent; sibling addressing is denied unless explicitly granted.

---

## Verified harness facts

Version-stamped. **Re-verify before relying on anything here** — these tools auto-update and break
things. In one day of research, Gemini moved 0.40.1 → 0.53.0, Codex 0.145.0 → 0.146.0 (a scripted
Enter hit its update prompt mid-experiment), and Codex removed `wire_api = "chat"` outright.

Baseline: **Claude Code 2.1.220 · opencode 1.17.3 · Gemini CLI 0.53.0 · Codex CLI 0.145.0 (S3) and
0.146.0 (S5, alt-screen)**. Qwen Code and Amp claims are **unstamped and unverified locally**.

**Session ownership.** Neither Codex nor Claude Code locks a session. Two concurrent `codex resume`
processes on one id both start and neither is refused; writes are `O_APPEND` so records survive,
but the two conversations diverge irreconcilably (Codex rollout records carry `turn_id` and no
parent pointer). **marion must enforce single-writer itself.**

**Spawn surfaces.** Four presets — `shared` (typed control *and* a live native TUI; Codex today),
`headless`, `interactive`, `opaque` — over three independent axes (control transport, display
surface, observation sources). `shared` is preferred wherever it exists.

**Terminals.** Claude Code uses the **alternate screen** for its entire session, so it needs no
scrollback handling. Codex uses the main screen — so all scrollback work is a Codex concern — and
erases scrollback (`CSI 3J`) on every resize, which marion must intercept. Codex does enter the alt
screen transiently for full-screen overlays (the `/diff` pager), so buffer switching mid-session is
required. Both harnesses emit terminal probes (DA1, XTVERSION, CPR); marion answers them, but our
own fixtures show both proceeding without answers, so this is prudence, not a requirement.

**Codex app-servers are never reaped**, but **unsubscribed threads unload after 30 minutes** and
`thread/start` does not materialize a rollout. `thread/resume` is the subscribe mechanism.

**Stop hooks can re-prompt a stopping agent** on both harnesses via
`{"decision":"block","reason":…}`. Codex hooks fail **silently** until trusted.

**⚠ `CLAUDE_CONFIG_DIR` isolation breaks OAuth** — the macOS Keychain entry is keyed to the real
config dir. Config isolation and subscription auth are mutually exclusive for Claude Code children,
which makes the fileless launch path load-bearing rather than merely preferred.

Full protocol details, launcher requirements, and per-harness caveats: design doc §5–§6.

---

## Resource model

Measured 2026-07-30 on macOS 26.5.1 / arm64 / 24 GB. `ps -o rss` overstates ~2× by counting shared
clean pages — use `phys_footprint`.

| harness | marginal footprint | startup | idle CPU | fds |
|---|---|---|---|---|
| `codex app-server` | **53 MB** | **40 ms** | 0.20% | 40 |
| `claude` idle | 182 MB | 0.8–1.8 s | 0.33% | 25 |
| `claude` **active session** | **462 MB** | — | — | — |
| `opencode serve` | 175 MB | 350 ms | **1.15%** | 28 |
| `gemini` (2 procs) | 345 MB | 3.0 s | 0.00% | 97 |

Extrapolated for 1 Claude root + N Codex children with sessions loaded: **~200 on 32 GB, ~440 on
64 GB** — valid for the **direct-MCP path only** (an Agent-tool shim per child adds ~462 MB, ~9×),
and measured with MCP servers disabled. Memory binds before fds or process limits.
**Context accumulation, not process overhead, is the real driver** — an in-use session is ~2.5× a
fresh one. macOS compression absorbs idle fleets well: 8 idle Claude instances grew the compressor
473 MB while total anonymous pages *fell*, with zero swapouts — so dormant harnesses cost less than
their footprint suggests, though an actively-inferring one's working set stays resident.

Rules: rasterize only the visible pane; coalesce high-rate output from unwatched agents; **reap
idle nodes** (never running ones) keeping transcript and ownership claim; expose a cross-harness
concurrency cap. **Do not use SIGSTOP as hibernation** — measured, it saves no memory at all. Sole
exception: `opencode serve` busy-polls at 1.15%/core while idle.

---

## Any harness × any model — verified viable (2026-07-30)

Model choice is a **separate plane from control**. No harness has a client-side model allowlist,
and per-process isolation works everywhere, so two children of the same harness can run different
models simultaneously. Anthropic documents the gateway path and does not prohibit it. The reverse —
routing third-party clients through Pro/Max OAuth — is prohibited and enforced; never do it.

| Harness | Override | Per-child isolation | Proxy must serve | Difficulty |
|---|---|---|---|---|
| opencode | `provider{}` JSON | `OPENCODE_CONFIG_CONTENT` (inline) | **nothing** — speaks all 4 wire formats natively | trivial |
| Claude Code | `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` | `--settings` / env (**not** `CLAUDE_CONFIG_DIR` — breaks OAuth) | Anthropic Messages, SSE mandatory | easy |
| Qwen Code † | `OPENAI_BASE_URL/_API_KEY/_MODEL` | `QWEN_HOME` | OpenAI Chat Completions | easy |
| Gemini CLI | `GOOGLE_GEMINI_BASE_URL` | `GEMINI_CLI_HOME` | Gemini `v1beta` + mandatory `:countTokens`, `:embedContent` | medium |
| Codex | `[model_providers.X]` | `CODEX_HOME` / `-c` inline / `-p` profile | **OpenAI Responses ONLY** | hardest |
| Amp † | — | — | — | **blocked** |

† unverified locally.

Order of attack for model injection: opencode → Claude Code → Qwen → Gemini → Codex. (Distinct from
adapter build order: claude-code → codex → acp → opencode.)

**Codex is hardest**: `wire_api="chat"` removed; sends `include:["reasoning.encrypted_content"]`
unconditionally; `apply_patch` only as a Lark-grammar custom tool; MCP wrapped in a proprietary
`type:"namespace"` item; startup gated on `GET /models` returning `{"models":[…]}`. Subscription
auth cannot use a custom `base_url` at all.

**Amp is structurally blocked** — BYOK removed, inference runs on Sourcegraph's machines.

marion need not build the proxy; LiteLLM / Vercel AI Gateway / OpenRouter translate. **Universally
expect to lose** prompt-caching fidelity (silently — `usage: 0`, not errors), reasoning-state
round-tripping, and accurate token counts.

---

## Chosen tooling

| Area | Pick | Version |
|---|---|---|
| PTY (Unix, async) | `pty-process` (`features = ["async"]`) | 0.5.3 |
| VT emulator | `alacritty_terminal` (+ `vte` `ansi`) | 0.26.0 / 0.15.0 |
| VT differential oracle | `avt` | 0.18.0 |
| TUI snapshot tests | `ratatui::backend::TestBackend` + `insta` | 0.30.2 / 1.48.0 |
| ACP client | `agent-client-protocol` | 2.0.0 |
| Reference ACP agent | `agent-client-protocol-test` → `testy` (git dep) | 0.11.0 |
| Canned model provider | `wiremock` + Codex's `core_test_support::responses` builders | 0.6.5 |
| Expect-style CLI tests | `expectrl` | 0.9.0 |
| PTY (Windows — deferred) | `portable-pty`, blocking only; needs a thread bridge | 0.9.0 |
| Fixture format | asciicast v3 NDJSON, hand-rolled serde (the `asciicast` crate died in 2018) | — |
| Seed VT corpus | vendor `vt100-rust` `tests/data/fixtures/` (MIT) | — |

`wezterm-term` is **not on crates.io**. `vt100` retained **0** scrollback lines in every real
capture and is disqualified.

**Prior art to port** (read-and-port, not dependencies): `openai/codex` →
`codex-rs/app-server/tests/common/mock_model_server.rs` (the canned provider) and
`codex-rs/app-server-test-client`; `agentclientprotocol/registry` →
`.github/workflows/protocol_matrix.py` (the `marion doctor` probe logic). **esctest2** is the real
automated VT conformance suite but needs a pty shim answering DSR/CPR/DECRQSS — later milestone.

**No harness ships an official mock or offline mode.** Base-URL redirection is the supported
mechanism, which is why the canned provider and the model-injection plane are the same component.

---

## Testing strategy

**E2E through real harnesses is the test**; only inference is canned. Layers L1–L6 and the
`marion doctor` conformance suite are specified in design doc §8. Two rules that are policy, not
implementation:

- **Every spike emits a fixture**, so answers become regression tests instead of evaporating.
  **This rule is currently violated and the violations are known:** the Gemini and opencode
  launcher findings and this document's entire resource model rest on uncommitted single sessions.
  Re-measure them, with fixtures, before treating them as settled.
- **Fixtures contain system prompts, repo contents, and anything secret that appeared in tool
  output.** Redaction pass plus a pre-commit secret scan are mandatory; prefer recording against
  the canned provider.

---

## Milestones

**All five spikes are resolved** (design doc §12). Build **M1 disposably** — prove the delegation
core before anything that displays it: no detached daemon, no VT emulator, no model proxy, no event
log beyond the task audit trail.

- **M1** — one real cross-harness hop over the direct-MCP path, returning a task contract, driven
  entirely by the canned provider.
- **M2** — supervisor split: TUI crash does not kill agents; reattach restores the tree.
- **M3** — tree UI + embedded terminal.
- **M4** — N→1 fan-in: a Codex root spawning two Claude children concurrently.
- **M5** — ACP breadth, degrading per resolved capabilities.

Post-M5: ModelProxy translation. Acceptance criteria for each: design doc §9.

---

## The strategic challenge (independent Codex review, 2026-07-31)

The strongest argument against this project, recorded so it need not be rediscovered.

**Its case.** The scope is at least five products — delegation broker, session manager, terminal
multiplexer, cross-vendor observability, model-routing proxy — and most of the plan is not required
to deliver the core contract. ACP already occupies the normalization layer with ~30 adapters, and
vendors are moving *upward* into orchestration natively. "Any harness × any model" is proxy
configuration plus documented losses, while expanding the security boundary to credentials and
inference traffic.

**Its sharpest point:** the volume of version-sensitive facts, and the retractions in design doc
§12, are evidence that marion proposes to own the union of every harness's compatibility burden
before writing a line of code.

**Its recommended wedge:** an ACP-first local delegation broker for cross-harness worktree
delegation with auditable results — ownership, diff attribution, replayable task contracts —
cutting the TUI, PTY emulation, native adapters, the universal IR, and the model proxy.

**Our assessment.** Accepted and applied: every technical correction, and the **task contract**,
which was the genuinely missing durable primitive. Accepted on sequencing: prove delegation before
building what displays it, hence a disposable M1. **Not accepted: cutting the TUI and model plane.**
Codex optimizes for the most defensible product; that is not the objective here. Watching and
clicking into running cross-harness subagents is the stated purpose, and any-harness × any-model is
an explicit north-star goal. The resolution is sequencing, not amputation.

**The risk we consciously accept:** owning several unstable integration boundaries at once. On a
second pass Codex sharpened this and we accept the sharpening: **the TUI and model plane are not
what will sink this — the adapter layer is.** The TUI is largely bounded work once the adapters
exist; the model proxy is substitutable (LiteLLM). Sustaining N adapters against tools that update
weekly is the unbounded cost, and twelve corrections in one day of research is the evidence.

Mitigations: `marion doctor --adapter` (behavioral contract tests, not just capability probes),
version-stamped claims, pinned binary paths, fixture-based drift detection. **The trigger for
narrowing to the ACP-first fallback is adapter churn, measured — not a decision made now.** The
expected form of that narrowing is deep support for two harnesses with the rest as community
adapters.

---

## North star

marion is not ultimately a wrapper. The harness-bridging layer is the foundation, not the product —
long term this is where multi-agent work is defined, executed, observed, and reasoned about,
independent of which vendor's CLI is underneath: its own agent/task model, its own execution
semantics, its own persistent state, its own interface as the primary place work happens.

Deliberately under-specified. It exists as a decision filter: **prefer choices that keep marion's
own model authoritative (its IR, its registry, its log, its task contract) over choices that make
marion a faithful mirror of one harness's concepts.** Concretely — do not let Claude Code's Agent
semantics become marion's semantics. Parity is right for M1, but parity *via translation into
marion's model*, not by adopting that model wholesale. The two look identical at M1 and diverge
completely by the time you want your own execution semantics.

## Explicitly out of scope (for now)

- **The graph-plan system** sketched in `info.md`: a DAG of plan nodes with executable per-node exit
  criteria, a "node 0" validating the test infrastructure itself, and a red-before-green rule
  requiring each node's test be observed failing before the work and passing after. Good idea,
  separate product, would consume this one. The task contract is shaped so it can attach later.
- Arbitrary agent-to-agent mesh routing.
- Remote hosting. Local-first; remote falls out of the supervisor split later.
- Windows. `pty-process` is Unix-only and `portable-pty` has no async.
