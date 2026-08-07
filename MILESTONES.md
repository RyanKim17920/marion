# marion — Goals & Milestones

A meta-harness: run any agent harness, on any model, as a first-class subagent of any other
harness — with one UI over the whole tree.

> **Scope of this file:** goals, principles, verified harness facts, and **milestone status**.
> **Operational detail lives
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
9. **Direct-MCP spawn is the primary delegation path.** The parent calls marion's `spawn` tool (spelled per harness; design §3.1 item 1) and
   receives a structured task contract. The Claude Code Agent-tool shim is optional sugar — it
   launders results through an extra LLM turn and costs a full ~462 MB process per child.
10. **Delegation must be auditable.** Every hop produces a task contract: ownership, base commit,
    acceptance criteria authored *before* the run, declared writable scope checked against observed
    writes, verification evidence, and the resulting diff. Prose from a foreign agent is not a
    result.
11. **A node is not done while its children are running, and a status update is not a delivery.**
    Completion is explicit *and* descendant-gated: an agent may deliberately report early (marked
    as such, with the still-live children listed), but it may not exit without choosing. *This
    binds nodes that **owe a result**. A root owes none — no requester, no contract — so its exit
    is accepted directly **once no descendant is live**, and it is never given a report tool at all
    (design §7.6 step 1). A root that stops with live descendants is still held, and can land
    `held_to_timeout` — descendant-gating binds it exactly as it binds any other node.* Crucially,
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

Baseline: **Claude Code 2.1.220 · opencode 1.17.3 · Gemini CLI 0.53.0 · Codex CLI — 0.145.0 for S3
and the alt-screen/`/diff` capture, 0.146.0 for S5**. (The 0.146.0 captures contain no `?1049h` at
all; the alt-screen evidence is the 0.145.0 file.) Qwen Code and Amp claims are **unstamped and unverified locally**.

**A version here names the binary a behaviour was measured on, not the binary that will run.** The
installed `claude` on this machine is **2.1.223**, not the 2.1.220 stamped above and throughout this
file. `5792140` replaced the exact pin with a **set** — `marion_testsupport::PINNED_HARNESSES`, the
one table every version check in the workspace reads — whose **entry zero never moves**, because
entry zero is what the prose claims, and whose tail carries versions since observed green with the
evidence beside each. claude's set is `["2.1.220", "2.1.222", "2.1.223"]` and codex's is
`["0.146.0", "0.146.1"]` — both auto-updated mid-session on 2026-08-06, §7.7's hazard arriving
live, and each new version was admitted only after its probes were re-run and compared against the
committed captures field for field; gemini and opencode each pin exactly one. So
read "2.1.220" as *"the version the turn-one `\"tools\":[]` shape and the
`tests/fixtures/s9` `can_use_tool` frame were captured from"*, and read a green suite as *"and
2.1.222 and 2.1.223 were checked against them too."*

**How fast this goes stale, now that there is a rate rather than an anecdote.** On 2026-08-06 two
of the four pinned harnesses auto-updated underneath a single session — codex 0.146.0 → 0.146.1 in
the morning, claude 2.1.222 → 2.1.223 in the evening — and in the four days of S1–S17 the four
binaries moved seven times between them. **Assume a pinned version has days of life, not months**,
and read a red pin as the ordinary weather rather than as an incident. The maintainer's move is
fixed and is not "widen the set": re-run the probes that a green suite would *not* re-run, compare
against the committed captures, and add the version with what was compared beside it. The gap that
makes this necessary is that only `tests/fixtures/s9` and codex's `s6`/`s7` shapes are asserted by
a test at all — `s10`, `s11`, `s14` and `s16` are prose, and `child_stream`, `child_events` and
`node_attach` drive real harnesses without going through the gate — so a green suite is always the
weaker half of the evidence. Two conveniences make the re-measurement cheap and are worth
preserving: every spike keeps a runnable probe under `spikes/`, and both CLIs keep old releases
side by side under a repointed symlink, so **the previous version is still on disk and one axis can
be varied at a time**. That is how 2.1.223's widened SIGKILL grace was told apart from machine load
and how s11's `system/hook_progress` divergence was told apart from a protocol change.

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
required. Both harnesses emit terminal probes but **different sets** — Claude Code sends DA1 and
XTVERSION; Codex sends DA1, CPR, and OSC 10/11. marion answers all of them, but our own fixtures
show both proceeding without any answer, so this is prudence, not a requirement. **That is a
statement about the TUI path only:** measured 2026-08-03 (S11), **headless `claude -p` emits no
probes at all** — on any of four fd topologies, including one where it owned the pty as its
controlling terminal.

**⚠ A pty changes the *framing* of a harness's stdout, not its protocol — and `claude -p` refuses
a pty stdin outright.** *(Claude Code 2.1.220, macOS darwin 25.5.0, spike S11, 2026-08-03,
`tests/fixtures/s11/`.)* Replaying S1's interrupt script verbatim over a pty gives an **identical
frame sequence and 36 of 36 non-delta frames byte-identical** to the pipe run. But the same bytes
arrive as **139 reads on a pipe (largest 46,515 B, none fragmentary) and 230 on a pty (largest
1,024 B, 40% of them containing no line terminator)** — so **any consumer of a harness's stream
MUST buffer and split on frame boundaries and MUST NOT treat a read as a frame**; that code is
correct on a pipe and broken on a pty. The `\r` a pty adds is `ONLCR` and is a **separate**
mechanism from the chunking — clearing `OPOST` removes every `\r` and leaves the read count
identical — so fixing the `\r` fixes nothing about the framing. Separately, **a launcher MUST NOT
give a headless node a pty on stdin**: `claude -p` exits **1** with *"Input must be provided
either through stdin or as a prompt argument when using --print"*, isolated to `isatty(stdin)` by
a pty-stdin/pipe-stdout capture. Design doc §5.2, §6.4, §11 item 1.

**Codex app-servers are never reaped**, but **unsubscribed threads unload after 30 minutes** and
`thread/start` does not materialize a rollout. `thread/resume` is the subscribe mechanism.

**Stop hooks can re-prompt a stopping agent** on both harnesses via
`{"decision":"block","reason":…}`. Codex hooks fail **silently** until trusted.

**⚠ `CLAUDE_CONFIG_DIR` isolation breaks OAuth** — the macOS Keychain entry is keyed to the real
config dir. Config isolation and subscription auth are mutually exclusive for Claude Code children,
which makes the fileless launch path load-bearing rather than merely preferred.

**⚠ `CODEX_HOME` isolation breaks auth too, but is recoverable.** Codex keeps its credential in a
plain `0600` `auth.json`, **not** the Keychain, so copying that one file into the isolated dir
restores auth completely — the fileless path is **not** load-bearing for Codex. The price is that
every seeded node holds a live copy of the user's OAuth tokens: per-node dirs are `0700`, never
uploaded or archived, and shredded on teardown. Still open: refresh-token rotation across copies,
and `GEMINI_CLI_HOME`, which may behave like Claude Code rather than like Codex.

**⚠ Claude Code connects `--mcp-config` servers asynchronously and does not hold the first turn
for them.** *(Claude Code 2.1.220, macOS darwin 25.5.0, measured 2026-08-02 during M1.)* Its own
debug log says so. Against a **real** endpoint the race never shows — the model takes seconds, the
connect ~70 ms. Against a **canned or otherwise fast** endpoint the reply returns in microseconds,
the first request goes out with `tools: []`, `mcp__marion__spawn` is never offered, the provider
correctly reads a toolless request as the session-title request, and the root emits a title and
**exits 0 in 63 ms with no error anywhere** — a silent success that does nothing. This is a
property of the launch protocol, not of the canned provider, so **any launcher driving Claude Code
headlessly against a fast or mocked endpoint MUST gate the prompt**: withhold it until the bridge
has *flushed* its `tools/list` reply (the marker is written after the flush, not at process start),
then complete a `control_request`/`control_response` `initialize` round trip so the harness's event
loop has demonstrably run since — **no sleeps** — and refuse the run with a named error if the
marker never appears. Design doc §6.1 step 8.

**⚠ `codex exec` 0.146.0 leaks a background `git fetch` that outlives the process.** *(codex-cli
0.146.0, macOS darwin 25.5.0, measured 2026-08-02 during M1.)* It starts a curated-plugin-
marketplace clone into `$CODEX_HOME/.tmp/plugins-clone-*`; the fetch **survives the exec process**,
reparents to pid 1, keeps writing into the agent dir being torn down, and reaches the network on a
run premised on making no network calls. **It is not reapable after the fact** — the descendant
sweep enumerates before the child dies, and this is already an orphan by then — so a launcher
isolating `CODEX_HOME` **MUST** write `[features] plugins = false` into the child's `config.toml`.
Related to but **distinct from** the `setsid` tool-call escape (design doc §11 item 18): that one
is a tool-call child escaping the process group during a run; this one outlives the run entirely.

**`--setting-sources ""` suppresses plugins and user hooks — not slash commands, agents or
skills.** *(Claude Code 2.1.220, macOS darwin 25.5.0, measured 2026-08-02 during M1.)* With the
flag set, `system/init` still listed 15 slash commands, 5 agents and 15 skills, but **0 plugins and
no user hooks**. That is the pair that matters for isolation and cost, and MCP tools and the turn
were unaffected — but the design doc previously implied a clean sweep. **The counts are this
machine's configuration and are illustrative**; the durable finding is qualitative.

**`codex exec` resends the conversation: the Responses `input` grows with prior turns** — measured
`ninput = 7 → 9 → 11` across a three-turn child *(codex-cli 0.146.0, 2026-08-02)*. Not settleable
from `tests/fixtures/s6/`, whose request log is reduced and strips `input`.

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
| Claude Code | `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` | `--settings` + `--setting-sources ""` / env (**not** `CLAUDE_CONFIG_DIR` — breaks OAuth) | Anthropic Messages, SSE mandatory | easy |
| Qwen Code † | `OPENAI_BASE_URL/_API_KEY/_MODEL` | `QWEN_HOME` | OpenAI Chat Completions | easy |
| Gemini CLI | `GOOGLE_GEMINI_BASE_URL` | `GEMINI_CLI_HOME` | Gemini `v1beta` + mandatory `:countTokens`, `:embedContent` | medium — **but blocked vendor-side on a personal login; see below** |
| Codex | `[model_providers.X]` | `CODEX_HOME` / `-c` inline / `-p` profile (isolating `CODEX_HOME` **also** breaks auth — but copying `auth.json` in fully restores it) | **OpenAI Responses ONLY** | hardest |
| Amp † | — | — | — | **blocked** |

† unverified locally.

Order of attack for model injection: opencode → Claude Code → Qwen → Gemini → Codex. (Distinct from
adapter build order: claude-code → codex → acp → opencode.)

**Codex is hardest**: `wire_api="chat"` removed; sends `include:["reasoning.encrypted_content"]`
unconditionally; `apply_patch` only as a Lark-grammar custom tool; MCP wrapped in a proprietary
`type:"namespace"` item; TUI/app-server startup gated on `GET /models` returning `{"models":[…]}` (**not** exercised by `codex exec` — verified 0.146.0). Subscription
auth cannot use a custom `base_url` at all.

**Amp is structurally blocked** — BYOK removed, inference runs on Sourcegraph's machines.

**⚠ Gemini CLI is blocked vendor-side for individual accounts** *(gemini 0.53.0, macOS darwin
25.5.0, reproduced 2026-08-05 with **bare `gemini -p` and no marion involved**)*. It never reaches
the model: `Error authenticating: IneligibleTierError: This client is no longer supported for Gemini
Code Assist for individuals. To continue using Gemini, please migrate to the Antigravity suite of
products`, with `reasonCode: UNSUPPORTED_CLIENT`, `tierId: free-tier`, **exit 55**. The Override /
isolation / proxy columns above are statements about the *mechanism* and are unchanged; what is
closed is the route to a **live** Gemini node on a personal `oauth-personal` login. Everything this
file measures about Gemini against the **canned** provider is unaffected — the sixteen matrix cells,
S12, the `trust: true` trap — because those never authenticate to Google. **Do not read a green
Gemini cell as evidence that a live Gemini child works.** Unblocking needs Vertex or a
`gemini-api-key`, which marion cannot choose for the operator without writing
`~/.gemini/settings.json` — design §6.4 forbids it. Antigravity shares `~/.gemini/` and owns the
`service=gemini` Keychain item S12 found, so S12 is a starting point, but the CLI surface must be
measured rather than assumed to carry over.

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
| Glob matching (writable scope) | `globset` (`literal_separator = true`; design §5.4 pins the dialect) | 0.4 |
| Fixture format | asciicast v3 NDJSON, hand-rolled serde (the `asciicast` crate died in 2018) | — |
| Seed VT corpus | vendor `vt100-rust` `tests/data/fixtures/` (MIT) | — |

`wezterm-term` is **not on crates.io**. `vt100` retained **0** scrollback lines in every real
capture and is disqualified — but that retention figure is **unverified**: no committed fixture
reproduces what either emulator retained, since no Rust exists yet (design doc §11 item 10). The
choice rests on `wezterm-term` being unpublished and alacritty modelling scrollback at all.

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
  **This rule is currently violated; the violations are enumerated in design doc §11 item 10**
  (item 12, S6, is closed — its fixture is committed at `tests/fixtures/s6/`; item 18, S7, is
  closed the same way, at `tests/fixtures/s7/`; item 3's Codex half is closed by S8, fixtured at
  `spikes/s8/`, whose report carries structural facts only — key names, file modes, exit codes —
  because the spike touched real OAuth credentials; item 14's `can_use_tool` third by S9 at
  `tests/fixtures/s9/`; item 2 outright by S10 at `tests/fixtures/s10/`, which commits its
  **negative** control alongside the positive one — a mis-shaped hook registration that fired
  nothing, warned nothing and exited 0, so the fixture proves the positive run was actually the
  hook firing; and item 1 outright by S11 at `tests/fixtures/s11/`, which commits its **control**
  transport alongside the measurement — the same script over pipes — because the finding is a
  *difference* between transports and is unreadable from the pty capture alone).
  Treat that list as authoritative and keep it current; do not re-enumerate it here.
- **Fixtures contain system prompts, repo contents, and anything secret that appeared in tool
  output.** Redaction pass plus a pre-commit secret scan are mandatory; prefer recording against
  the canned provider.

---

## Milestones

**Spikes S1–S7 [done]** — resolved (design doc §12), each with a committed fixture. **S10 [done]**
— the live `SubagentStop` confirmation §11 item 2 owed before M1, fixtured in
`tests/fixtures/s10/`. The event fires; its field set is exactly S4's 11-key `Stop` set plus
`agent_id`/`agent_type`/`agent_transcript_path`; `session_id` and `transcript_path` are the
**parent's**, so a hook reading `transcript_path` on a `SubagentStop` reads the wrong file; and
`decision: block` re-prompts a stopping subagent and **replaces** the answer the parent reads. It
also **corrected** design doc §7.6: `num_turns` is a root counter and says nothing about a
subagent re-prompt — `parent_tool_use_id` is the discriminator. A negative control (mis-shaped hook
registration) fired nothing and reported nothing, which is why "no error" is never evidence a hook
is wired. Four follow-ups remain, and `run_in_background: true` — untested, and the case §7.6
actually exists for — is now design doc §11 item **21**. **S11 [done]** — the pty re-confirmation
of S1 that §11 item 1 owed before M1, fixtured in `tests/fixtures/s11/`: S1's argv and stdin script
replayed **verbatim** over a real pty against Claude Code 2.1.220 and the canned provider, cost
**$0.00**, across four fd topologies with the pipe run committed as the control. **The protocol is
unchanged** — identical 38-kind frame sequence, byte-identical interrupt `control_response`, 36 of
36 non-delta frames byte-identical. **The framing is not**, and `claude -p` **refuses a pty
stdin**: both are stated as MUSTs above. S11 also narrowed two other open items — §11 item **11**
(S1's interrupt latency is no longer a single run; it is still one machine) and §11 item **20**
(the pty-host half of "no real-terminal coverage" is paid; the emulator/rendering half, item 9's
`ESC[6n` stall and the keystroke-injection submit check are not, so item 20 is **narrowed, not
closed**). **S8 [partial]**, **S9 [partial]**, **S12 [partial]** and **S13 [partial]** —
the spikes that did not close their questions outright; **do not read any of them as closed.**
**S9 answered only the `can_use_tool` third of "the inbound half of Claude Code's control
channel"** (design doc §11 item 14) — the decompiled design was right in every field it named and
`root::deny_response` was accepted verbatim on first execution, but **hook callbacks,
`request_user_dialog` and `control_cancel_request` are still designed on decompilation**. Nothing
M1 builds depends on those three; they are M2 work. Fixtured in `tests/fixtures/s9/`.
**S8 answered only the Codex half of "does config-dir isolation break auth"** — it does, but Codex's credential is a
plain file rather than a Keychain entry, so seeding a copied `auth.json` fully restores it and
`CODEX_HOME` isolation is keepable. That is a new **requirement on the launcher** for any Codex
child talking to a real endpoint (seed the credential; `0700` dirs; never upload; shred on
teardown) and a new **open risk** for long-lived nodes (refresh-token rotation across copies).
Nothing here changes a milestone's scope — M1's child runs against the canned provider and is
deliberately **not** seeded. Still open and not to be assumed: `GEMINI_CLI_HOME`. Mechanism and the
MUSTs are design doc §6.4, §9 and §11 item 3.
**S12 answered the Gemini half of that same question — COPYABLE**, fixtured in
`tests/fixtures/s12/`, measured 2026-08-03 against gemini 0.53.0 and a local canned endpoint at
**$0.00**. `GEMINI_CLI_HOME` relocates everything including credentials, and nothing in that set is
unfixable-by-copy on the same machine under the same user: the file-backed store derives its
AES key from `hostname + username` plus a **hardcoded** passphrase, so no OS secret participates.
It **corrected** design doc §6.4 and §11 item 3(c) — the `service=gemini` Keychain item cited as
grounds for suspecting Claude-Code-like behaviour belongs to the **Antigravity IDE**; the CLI's own
`gemini-cli-oauth` item does not exist. It also **fixtured** several §6.4 Gemini launcher claims
that §11 item 10 lists as unevidenced (the `selectedType` requirement, the trust-workspace gate),
and recorded a new silent-failure trap: without `trust: true` or `--yolo`, an MCP server's tools are
**omitted from the request body entirely** — no prompt, no error, a run that succeeds having done
nothing. **Still open and why this is not a close of §11 item 3:** whether copied `oauth-personal`
credentials refresh correctly in a child is unverified — the whole measurement ran on
`GEMINI_API_KEY`.
**S13 characterized opencode 1.17.3**, recorded in `tests/fixtures/s13/`, measured 2026-08-03
against the real binary and a canned local OpenAI-compatible provider at **$0.00** (a report only —
binary inspection plus short live probes produced no capture, the same deviation S12 declared). It
answers the **opencode half of §11 item 10** — those launcher findings had no committed fixture —
and **corrected** design doc §6.4: that bullet describes the `opencode serve` HTTP path
(`POST /session/{id}/prompt_async`, `/event`, `OPENCODE_EXPERIMENTAL_EVENT_SYSTEM`), whereas the
path an adapter actually needs is `opencode run --pure --format json`, which **binds no TCP port**
and emits NDJSON on stdout. It produced four §12 correction rows (60–63), one a deliberate
**negative** result: opencode's MCP tool permission default is **allow**, so unlike codex and gemini
there is no silent-omission trap — recorded so that family is not over-generalized. Two hazards an
adapter must handle: opencode **never exits** on a provider hang (a 500 still retrying at 90 s, a
connection-refused still hung at 180 s), which makes §9's two-step group kill **load-bearing rather
than defensive**; and **cross-harness contamination** — opencode reads `~/.claude/CLAUDE.md`,
project `CLAUDE.md` files and `~/.claude/skills/**`, so a marion-spawned opencode child inherits a
*different* harness's user configuration unless severed. **Why this is [partial] — narrowed
2026-08-04, and two of its three original grounds are now gone.** An opencode adapter exists
(`crates/marion-harness/src/opencode.rs`) and opencode has since run as a marion child **and** as a
marion root, end to end, in every cell of `cross_product` and `journal_wiring` that names it. That
exercise also found a real defect the characterization had not predicted — an opencode child
resolved its project directory from `$PWD` and so worked in the operator's own repository rather
than its worktree (fixed in `8a69f22`; design §12). What keeps it **[partial]** is the remainder:
several questions are still **unknown** — whether pacote's git-plugin path honours `ignoreScripts`,
whether LSP children get an explicit kill, whether the `~/.claude/ide` lock scan is gated, whether
`--port` on `run` is truly unconsumed — and the three original server-path claims remain unfixtured,
because S13 did not re-measure them. **S7 answered NO** —
`codex exec` does **not** keep its tool-call children in marion's process group; it `setsid`s each
one, so a single `killpg` at timeout expiry leaks every runaway tool-call subprocess to pid 1.
marion's timeout kill must sweep the child's descendants for their process groups *before*
signalling. That is a requirement on M1's kill path, not a change to any milestone's scope; the
mechanism is design doc §9 and §11 item 18. **S6 answered YES on all three questions** — `codex
exec` does host MCP servers, so M1's `report` return path exists and M1 takes the **primary branch**
(marion's own `report` tool over MCP) rather than the `--output-schema` fallback. On Codex that tool
is reached by the `mcp__marion` **namespace** form, never the flat name (design §3.1 item 1).
Design doc §9 specifies what M1 builds. Build **M1 disposably** — prove the delegation core before anything that
displays it: no *detached* daemon (the registry, the task audit trail and the control MCP run
in-process), no VT emulator, no model proxy.

**Status convention** — introduced 2026-08-03, applied to every milestone and spike in this file:
**[done]** every acceptance criterion in design doc §9 is met; **[partial]** some criteria met and
the rest named; **[open]** not started. A milestone is never **[done]** while a criterion it names
is unmet — including the evidence criteria. Statuses are claims about *verified* state, not about
how much code exists.

- **M1 [done]** — one real cross-harness hop over the direct-MCP path, returning a task contract,
  driven entirely by the canned provider. Preceded by S6. **All seven of design doc §9's
  acceptance criteria are met**: the six functional ones, and the seventh — the evidence line —
  whose last debt (the pty re-confirmation of S1, §11 item 1) was paid on 2026-08-03 by **S11**.
  **[done] here means exactly what the convention says and nothing more: every criterion M1 names
  is met and verified.** It does **not** mean marion is usable for real work — read the two
  sections below, in order, before quoting this line.
- **M2 [done]** — supervisor split: TUI crash does not kill agents; reattach restores the tree.
  **All four of §9's criteria are met and verified as of 2026-08-07**, criterion 4 last, and the two
  caveats it carries are named at the bottom of this entry rather than left to be rediscovered. The
  paragraphs below are kept in date order because each records what was true when written, and three
  of them are now history.
  **Still [open] as of 2026-08-05 and not upgraded**, though its substrate moved twice: the journal
  is written and provably replayable (all sixteen pairings, `fdf2e7a`), and as of `8a16427` it is
  also **read at runtime** — `crates/marion-supervisor/src/watch.rs`, polled from a thread in
  `marion.rs:1018-1036`. "Written is not read" is retired: it is read, on one path, for one
  purpose. That purpose is the **live watch view**, not reattach, and none of §9's four M2 criteria
  is a view. What keeps M2 [open] is that **reattach restores nothing**: there is no socket, no
  registry at runtime, no `events.jsonl`, and no client to attach — see "What is not built" below.

  **Updated 2026-08-06, and the update makes M2 further from done rather than nearer.** A detached
  `marion-supervisor` now exists (`0d1224e`, `2349bb6`): it is started on demand, it holds §2's
  socket, it tails the journal, and it answers `node/get`, `tree/subscribe` and `session/quit`.
  **That is a reader and not an owner.** It holds no node's `Child`, pid, pipe or channel —
  `marion run` holds every one of them for the whole of a root's turn — so **§9's M2 criterion 1
  ("`marion-tui` SIGKILLed mid-run; agents keep running") is unmeetable today**, because the process
  that would have to survive the kill is the one holding the agents. **Criterion 4 (the clean
  quit-and-return) is unmeetable for the same reason** and says so itself: its load-bearing half
  requires a new client to receive events emitted *after* it attached, "proving re-subscription to a
  live channel rather than replay of a corpse", and there is no live channel on the supervisor's
  side. §11 item 28 states the gap in full and names the four changes that would close it. Anyone
  quoting the existence of a detached supervisor as progress toward M2's crash criterion is quoting
  the wrong half of it.

  **Updated 2026-08-07 — the paragraph above is now history: criterion 1 is measured, and with it
  the *mechanism* criterion 4's clause (ii) depends on. Criterion 4's own scenario came later the
  same day and has its own bullet below.**
  §11 item 28 **step 6** landed: `marion run` is a socket client. It sends `agent/spawn` with
  `caller: None`, the supervisor owns the root's thread, `Child`, pipe and pid, and the client
  attaches and renders over `node/attach`. The three sentences that made both criteria unmeetable
  are each false now — the supervisor *is* an owner, a root's `Spawned` carries a real pid written
  at `command.spawn()` rather than after the turn, and there *is* a live channel, because
  `node/attach`'s subscribe leg pushes from `events.jsonl` on the same cursor as its replay.
  - **Criterion 1** — `crates/marion-supervisor/tests/client_run.rs::a_killed_client_leaves_its_agents_running_and_a_new_client_sees_the_whole_tree`.
    SIGKILL the client mid-run; the root's pid stays `Liveness::Alive` (S15's three-valued
    `procid`, never `kill(pid,0)`), the supervisor is the same process by pid **and** the kernel's
    start identity for whatever wears it — `procid::resolve`'s comparison, not a restatement of it,
    so the reissued-pid property has a named test of its own — a second client's `tree/subscribe`
    renders both nodes, and the root's
    `events.jsonl` is **longer after the kill than at it** — which is what stops a zombie
    satisfying the criterion.
  - **Criterion 4's load-bearing half** — the same test. The new client's replay is contiguous
    from ordinal 0, its live leg is contiguous from the replay's read point (no gap, no repeat at
    the seam), and the event it asserts on is caused by a provider request the root made *after*
    the attach: the node's closing turn is held at the canned provider until the attach has
    happened, and the ordering is read out of the provider's own request log rather than out of a
    sleep (§6.1 step 8 binds a test as hard as it binds a launcher).
  - **Step 5 has landed too, so a child's owner is the supervisor as well.** The per-child MCP
    bridge dials §2's socket and sends `agent/spawn` with the capability token marion wrote into
    its node's declaration; it starts nothing, and there is deliberately no in-process fallback.
    What that buys is measured rather than argued:
    `crates/marion-supervisor/tests/background_spawn.rs::a_bridge_killed_mid_child_leaves_the_node_running_and_its_stream_growing`
    SIGKILLs a bridge mid-child and requires the node's own `events.jsonl` to **grow afterwards** —
    liveness, not presence in the process table. §11 item 30's *bridge*-death runaway is closed;
    the supervisor-death half is not.
  - **Criterion 2** — `crates/marion-supervisor/tests/client_run.rs::a_new_clients_tree_is_the_journal_the_supervisor_read_including_the_window_no_client_saw`,
    added 2026-08-07. Stated structurally, per §9, because no node M2 ships carries source-side
    ordering evidence — the test asserts `last_src_seq` is `None` so a future adapter that does
    carry it forces someone back to add the per-node form §9 owes that surface. §9's four things are
    four assertions, compared against a replay of exactly the `read_point.records` the supervisor
    handed back with that snapshot, never against the pre-kill tree. What makes it a measurement
    rather than a tautology: a node is mid-turn at the kill, and a second node **terminates during
    the detached window**, so replay has to pick up a terminal state and a contract no client ever
    saw. Each of the four has a corroborating reading that shares no code with the tree — the agent
    directories, the contract files, the contract's `requester`, the pid — because a mutation inside
    `Replay` moves both sides together and two of them proved exactly that.
  - **Criterion 3** — `crates/marion-supervisor/tests/client_run.rs::after_a_supervisor_sigkill_every_process_on_the_record_is_accounted_for`,
    added 2026-08-07, closing the *"no untracked live process"* half. `Spawned` now carries an
    opaque, platform-tagged start identity beside its pid, read in the `on_started` hook while
    marion still holds the `Child` — the one instant where the read is race-free — so replay can
    tell a survivor from a recycled pid. `procid::Claim` is three-valued and **cannot-tell blocks
    the claim**; it is never rounded up. *"Untracked"* is §11 item 30's sense — a process nothing
    will attend to — not *"any live process"*, since §7.2 sanctions a running orphan and a
    SIGKILLed supervisor leaves its fleet running by design. The `ReapedIdle`-resumable and
    `Live` → `Orphaned` halves were already covered in `restart.rs`.
    **Linux is not shipped**: `/proc/<pid>/stat` field 22 is designed and unmeasured, so non-macOS
    returns an explicit refusal that resolves to cannot-tell, and the consequence is tested.
  - **Criterion 4** — `crates/marion-supervisor/tests/client_run.rs::a_client_that_quits_cleanly_leaves_the_supervisor_running_and_a_new_client_resubscribes`,
    added 2026-08-07, and it is what moved M2 from [partial] to [done]. The bullet above credits the
    criterion-1 test with criterion 4's *mechanism*, and that is all it can be credited with: it
    **SIGKILLs** its client, and §7.3.1 exists precisely to separate a client that said it was
    leaving from one that vanished, so the *scenario* — §9's *clean* quit-and-return — was unmeasured
    until this test. Its client is one the test owns: it creates the root with
    `agent/spawn { caller: None }`, leaves through the real `session/quit` with disposition **(b)**
    (checked against the supervisor's own `Detached` answer, since (a) answers `Killed` and (c)
    answers `ReapedAndDetached`), and is **never signalled**.
    One run covers the whole criterion, including §9's *"run it also with a node that terminated
    during the detached window"*. The gate holds the root's closing turn, so at the quit the child is
    running and the root is inside the `spawn` waiting for it — §9's *at least one node mid-turn* —
    and across the window the child runs to completion and exits, the root's `spawn` returns, and the
    root parks at the held turn. (i) is asserted as a **standing invariant of the window** rather than
    a snapshot after it, so a supervisor that left is named in about a second instead of surfacing
    31 s later as the expiry of a wait for work it was supposed to be driving. (ii) is an event
    caused by releasing an answer the provider was still holding at the instant of the attach — the
    provider's own request log says the closing turn was asked once, before the attach, and not again
    after. (iii) is replay contiguous from ordinal 0, a read point equal to what was delivered, and a
    live leg contiguous from that point. The terminated child's replay is asserted to carry **both**
    bookends, because a non-empty contiguous replay missing its terminal record is §7.3.3's
    *"indistinguishable from one cut mid-turn"* and satisfies every other clause.
    Mutation-checked, each killing this test by assertion in under 8 s: disposition (b) served as (a)
    or as (c); a supervisor that reads a clean quit as its own cue to go (clause (i), 0.95 s); an
    attach that serves replay and then goes quiet (clause (ii), 7.3 s); a seam that drops or repeats
    a record (clause (iii), 1.8 s); and the terminated node's journal tail withheld (1.8 s).
  - **Two caveats criterion 4 carries, stated because neither is visible from the test's green.**
    **First, §5.7's zero-clients clause is structurally unreachable and is therefore not what clause
    (i) measures.** Each node's MCP bridge is itself a socket client of the supervisor (§11 item 28
    step 5), so while any node is running the connection count never falls to zero; §5.7's exit is
    gated on zero clients, and §7.3.2 waives the grace outright for a client that announced itself.
    "No client exists" in §9's sense means *no operator's client*, which is the sense criterion 1
    also uses, and that is what is measured. What makes clause (i) falsifiable is the remaining
    proposition — *a supervisor must not read a clean quit as its own cue to go* — and the mutation
    above is exactly that. A test of §5.7's zero-clients-with-a-live-node rule would need a marion in
    which a live node holds no connection, which is not this one.
    **Second, a reissued pid cannot be arranged by a test**, since pid reuse is the kernel's to
    schedule. It gets a control instead —
    `a_supervisor_wearing_a_reissued_pid_is_not_the_supervisor_that_was_there_before` — which
    presents the identical evidence against a genuinely serving supervisor: its recorded pid paired
    with a start identity that is real and belongs to another live process. A build whose fingerprint
    degenerated to a pid, or in which `procid::resolve` stopped treating a mismatch as evidence,
    fails there in 0.4 s.
- **M3 [open]** — tree UI + embedded terminal.
- **M4 [open]** — N→1 fan-in: a Codex root spawning two Claude children concurrently.
- **M5 [open]** — ACP breadth, degrading per resolved capabilities.

Post-M5: ModelProxy translation. Acceptance criteria for each: design doc §9.

### M1 — where it actually stands (2026-08-03)

**The delegation core works.** A real `claude` root calls `mcp__marion__spawn`; a real `codex`
child starts in a worktree, edits a file, and returns through marion's `report` tool over MCP (S6's
primary branch); the root receives the structured contract as a tool result; a deliberate
out-of-scope write is caught detectively; the whole run is canned. `cargo test --test m1_hop`,
~4.7 s *(measured 2026-08-04; **570 tests** across five crates at HEAD `08b4fde`, up from 138 on
2026-08-03 — see
"The evidence base" below for what that growth is and, more importantly, what it is not)*.
The contract criterion is asserted on the
**request** side as §9 requires — the recorded `tool_result` is not a `<persisted-output>` stub, it
deserializes, and it equals the persisted `contracts/<task_id>.json` modulo the cap rules'
shortening and the metadata recording it.

**All six functional criteria are met.** The last to land was §9's timed-out-descendant criterion
— `kill(pid, 0)` returns `ESRCH` for every pid in a **non-empty** enumerated descendant set —
asserted in `crates/marion-supervisor/tests/timeout_kill.rs` over the set marion enumerated in step
1 of its two-step group kill (S7's ordering).

**Criterion 7, the evidence line, is now also met — the ledger closed on 2026-08-03.** Design doc
§9's *"Owed here"* named three debts; all three are paid, each with a committed fixture. Item 14
(a real `can_use_tool` round-trip) by **S9**. Item 2 (the live `SubagentStop` confirmation) by
**S10**. And item 1 — **the pty re-confirmation of S1** — by **S11**, which replayed S1's argv and
stdin script verbatim over a real pty and found the interrupt protocol **byte-for-byte unchanged**,
while turning up two things that were not what the debt was about and are now MUSTs: a reader must
not treat a `read()` as a frame, and a headless node must not be handed a pty stdin. **So the
headline is seven of seven criteria met, and M1 is [done].**

**What that sentence does not say.** M1 was scoped to prove exactly one thing — that a real
cross-harness delegation hop returns an auditable contract — and was built **disposably** to prove
it (design doc §9). Its criteria are a deliberately narrow bar, and clearing them says nothing
about the rest of the system. **"M1 [done]" must never be read as "marion works."** The journal
does not exist, so nothing survives a supervisor restart. Descendant gating — which principle 11
above calls non-negotiable — is specified and **not in code**. `verification` never executes, so
every contract's `evidence` is empty. The next section is the authoritative list, not a footnote
to this one.

### Not started — what a working M1 does *not* imply

**Read this before treating M1 [done] as a statement about marion.** M1 was built disposably on
purpose (design doc §9), so a working hop — and a met acceptance criterion — rests on very little.
None of the below is covered by any M1 criterion. **Re-checked line by line against the tree on
2026-08-04**; each entry now says whether it moved, and all but one did not. Where an entry says
"unchanged", that is a verified claim rather than an unrevised one.

- **The journal (design §4.3) — REVISED 2026-08-04, revised again 2026-08-05.**
  The journal is **written**: `SpawnIntent`, `Spawned`, `Exited` and `ContractPersisted`, plus
  `SpawnAborted` and `PermissionDenied` conditionally, from both processes that create nodes, and
  `journal_wiring.rs` proves a real run's records replay to the tree that run left on disk for
  **all sixteen** root/child harness pairings. It is now also **read**, which retires the sentence
  this bullet used to turn on. `8a16427` added `crates/marion-supervisor/src/watch.rs` — a byte
  cursor over the append-only file handing new bytes to `marion_core::registry::replay`
  (`watch.rs:144`) — run from a polling thread started before the root and joined after it
  (`crates/marion-supervisor/src/bin/marion.rs:1018-1036`), with a guaranteed final poll because
  `follow_journal` (`marion.rs:853-869`) reads the stop flag *before* its poll and returns after
  it. **Precision, because the two are easy to conflate:** `registry::replay` has a second
  production-code caller — `journal::read_path` → `journal::read`
  (`crates/marion-supervisor/src/journal.rs:218-228`) — but nothing reachable from a binary calls
  `read`, so `watch.rs` is the **only journal reader on a live run path**. What it buys is a
  watcher seeing a child while it runs, and that is all it buys: a supervisor restart still
  recovers nothing, `Orphaned` marking and reap recovery still have no substrate, and **M2's
  replay criteria remain unmet**. This bullet stays on the list for that reason, with a different
  reason than it had yesterday.
- **The registry.** Unchanged. `marion_core::registry::replay` exists as a pure unit and is
  exercised only by tests.
- **Descendant gating (§7.6) is not in code** — principle 11 above is specified, not enforced.
  Unchanged, and now visible in the artifact: `spawn.rs` writes `live_descendants_at_report:
  vec![]` on every contract, so the field a gate would read is a hardcoded empty list.
- **`verification` command execution, so a contract's `evidence` is always empty.** Unchanged, and
  as of `77557e3` a `spawn` carrying `verification` is now **refused by name** rather than accepted
  and dropped — the commands still never run, but a caller can no longer be told they did.
  Design §11 item 23.
- The `Stop` hook path. Unchanged — no production code references it.
- `marion doctor --adapter`, `marion-term`, `marion-tui`, `marion-proto`. Unchanged; `doctor` is
  still the stub that prints "no adapters registered yet".
- `status` / `wait` / `list` appear in `--allowedTools` but are **not implemented** — the bridge
  declares only `spawn` and `report`. An allowlist entry for an undeclared tool is inert.
  Unchanged, and deliberate: design §9 records the resulting `ntools=2` so a reader does not
  misdiagnose it as a tool-compilation failure.
- **`isolation` and `background`, added to this list 2026-08-04.** Neither was ever implemented —
  `run_spawn` creates a worktree unconditionally and `spawn` blocks — and both were previously
  *accepted and ignored*. Both are now refused by name (`77557e3`); `name` and
  `allow_concurrent_writes` remain accepted-and-dropped, deliberately, for the reasons design §11
  item 23 gives.

### What is not built — checked at HEAD `08b4fde`, 2026-08-05

The list above says what is not *done*. This says what does not *exist*, because the two read
differently and only the second explains why M2 cannot move. Each line is an absence verified in the
tree on this date, not an unrevised one.

- **No unix socket, anywhere.** Zero occurrences of `UnixListener` or `UnixStream` in the workspace.
  A supervisor a client could attach to is not partially built; it is absent.
- **`events.jsonl` is never written.** `crates/marion-core/src/paths.rs:147` is a path accessor and
  `paths.rs:259` is the test that it returns that name. No other caller, no writer, no reader.
- **No `marion-tui`, `marion-term` or `marion-proto` crate.** The workspace is five members —
  `marion-core`, `marion-provider`, `marion-supervisor`, `marion-harness`, `marion-testsupport`
  (`Cargo.toml`).
- **`marion doctor --adapter` does not exist.** The `marion` binary refuses any argv[0] but `run`
  (`bin/marion.rs:107`); `marion-supervisor doctor` is a `println!` of "no adapters registered yet"
  (`src/main.rs:23-25`).
- **`verification` execution is unimplemented, so `evidence` is always empty** — and since `77557e3`
  a `spawn` carrying it is refused rather than accepted and dropped.
- **`pid` capture is unimplemented.** The field exists on `Spawned` and `registry::replay`
  propagates it (`registry.rs:228`), but **every production writer sets `None`**: `run.rs:925`
  ("marion drove the process through a helper that owns the child and surfaces no pid — an absence,
  recorded as one") and `root.rs:678`. Every non-`None` value in the tree is in a test.

What does exist: `marion run <agent-type> --prompt …` launches the root; contracts persist uncapped
to `<agent-dir>/contracts/<task_id>.json` with the capped copy returned; timeouts are enforced with
a descendant-pgid sweep; scope is checked both preventively (at spawn) and detectively (git-derived).
Mechanism for all of it: design doc §5–§7.

### The evidence base — what 2026-08-04 changed, and what it did not

**No milestone status changed on this date, and that is the finding.** M1 was already **[done]** and
M2 remains **[open]**: at that date the journal was still not read at runtime (`8a16427` changed
that on 2026-08-05 without moving M2 — see the bullet above), the registry does not exist,
descendant gating is still not in code, and `verification` still never executes. Not one of §9's M2
acceptance criteria moved. What changed is the **evidence base** — how much of what this file claims
is now measured rather than asserted, and how much of it is measured *per harness* rather than once.

**Generality, proven rather than assumed.** Several claims held for one harness pairing and were
extrapolated to sixteen. That extrapolation is now retired in three places: the **depth gate** is
proven for all four harnesses (`06706a0`), **journal emission and cross-process writer identity**
for all sixteen root/child pairings (`fdf2e7a`), and every **LaunchOnly root** is driven with
per-harness evidence (`71c08e8`). The measurement worth carrying forward is an asymmetry that
extrapolation would have hidden: **claude-code never reaches the depth gate at all** — its
`allowed_tools` denies `spawn` first, so the refusal that protects the other three is unreachable
on the one harness that reads that list.

**Nine real defects, found by making the tests general — this section said "one" and was wrong.**
Eight changed production code; the ninth (`6803b5b`) changed only tests and is listed because what
it closed was a live hazard, not a style point. Every one is the same family: **the contract, or
marion's own reply, claiming something other than what happened.**

1. An **opencode child worked in the operator's own repository rather than its worktree**
   (`8a69f22`): it resolves its project directory from `$PWD`, which marion had left inherited.
   §6.7 derives `changed_paths` by diffing the worktree the child never touched, so the contract
   read `{"status":"Ok","changed_paths":[],"scope_enforced":true}` — a clean bill of health for a
   run that wrote outside its worktree, outside the repo, and outside every scope list. This is the
   failure mode principle 11 and §6.7 exist to prevent, produced by marion itself, and it was
   invisible while only one pairing wrote anything.
2. **claude and gemini children had no write route at all** — filed as design §11 item 24, fixed by
   `3c76cae`. `--tools ""` was hardcoded because `AgentType` had no `tools` field to populate it
   from, and a declared tool was denied anyway because `allowed_tools` was exactly `[report]`. Such
   a child reports `Ok` with `changed_paths: []` and `scope_enforced: true` — **byte-identical to
   the escaped-write signature item 1 exists to catch**, which is why this one had to be fixed
   before the matrix could mean anything. `3c76cae` is §3.1's availability axis.
3. **`allowed_tools` was hardcoded to `["apply_patch", "shell"]` on every harness** (`45df64a`) —
   neither the requested value nor the compiled one, a constant left from when codex was the only
   child, so a gemini child's contract asserted two tools gemini has never had. Third instance of
   the same defect, after `harness` and `model`.
4. **`diff_text` omitted an intent-to-add** (`a262887`): it was `git diff <base> HEAD` ++ `git diff
   HEAD`, and with nothing committed the first term is empty while the second cannot see an
   untracked path. A child that *created* a file produced `diff: None` while `changed_paths` — which
   does implement §6.7's three-term union — correctly named it, so the contract attested to an edit
   whose bytes survived nowhere after the reap.
5. **`result_commits` was dropped in transit** (`247bf81`) — see the paragraph below.
6. **`--base-url` was accepted and dropped** (`b4283b4`) — see the paragraph above.
7. **Three `spawn` parameters declared in the schema and silently dropped** — `background`,
   `isolation`, `verification` (`77557e3`, design §11 item 23).
8. **A root's `report` was answered `report recorded`, `isError: false`** (`7ff470e`). §5.4 makes
   `report` self-only and only on a node that *has* a contract — rejected on a root — and nothing
   enforced that anywhere a root could reach. The permission axis omits the verb, but only the
   Claude Code adapter compiles `allowed_tools` into anything, so on codex, gemini and opencode a
   root's `report` reached the bridge and got a receipt for a payload nothing stages, after which
   the root exited `Ok` having delegated nothing. Found by reading §5.4's authorization table
   against the code, not by a failing test. The refusal now sits at the execution point — the one
   place all four harnesses pass through — which is the same move `check_spawn_gates` made for the
   child's mirrored hole.
9. **`Auth::as_wire` and `Auth::from_wire` were never asserted to be inverses** (`6803b5b`). Each
   carried its own `"inherited"` literal; all four adapters serialise through the first and the only
   reader goes through the second. Editing one alone breaks the hop **in the silent direction** — an
   unrecognised value falls back to `Canned`, so a live root's child goes canned with nothing
   reporting it. No production code changed; what changed is that the drift is now caught.

**`result_commits` — declared, dropped, now carried and capped (`247bf81`).** The field was in
`report`'s schema and read nowhere: dropped in the bridge, absent from `StreamOutcome` and
`ChildOutcome`, hardcoded empty in `build_contract`. So a child that committed its work and reported
its oids had them thrown away in transit and the contract then asserted it had committed nothing — a
**wrong answer rather than a gap**, because empty is exactly how a reader learns nothing was
committed, and `worktree_reap.rs` reads it that way. It is now threaded through the stream (one
derivation, `stream::report_commits`, four wire spellings; absent, `null` and `[]` are one claim,
because §9 has marion spell optionality as nullability for codex's `strict: true` schema) into
`Completion::result_commits`. **The cap shipped in the same change because the threading breaks
§6.7 rule 6 without it** — rule 6's terminal stub is justified by a field set whose size does not
depend on the input, which was true only while this field was always empty; measured, a pathological
contract encoded to 216,315 bytes against a 49,152-byte backstop. It is elided at **rule 5(d)** with
a `result_commits_omitted` count mirroring `changed_paths_omitted`, and **cleared at rule 6**
(`crates/marion-core/src/cap.rs:163-165, 209-221`); either alone converges, so 5(d) is the graceful
path and 6 is the guarantee. **A retraction belongs here:** an earlier claim that a child's work had
no durable route was overstated. The work was never lost — `changed_paths` has a committed term,
`diff` is against `<base_commit>`, and `workspace.branch` survives the reap still holding the
commits. What was lost is the **immutable handle**, since a branch is a mutable ref and an oid is
not. **And it is not yet exercised end to end:** no run produces a non-empty `result_commits` today.
Every canned fixture that drives a real hop sends `[]` (`cross_product.rs:371`,
`journal_wiring.rs:187`, `child_stream.rs:78`); the non-empty cases live only in unit tests of the
parser and the cap.

**All sixteen matrix cells now write.** They did not when this section was first written. `8a69f22`
found that twelve of the sixteen had children that never wrote anything — `Script::default`'s patch
applied only on the Responses wire — so `changed_paths` was empty *by construction* for claude,
gemini and opencode children, and an empty `changed_paths` is also what an escaped write produces.
The two were indistinguishable and the matrix could not tell them apart. After `8a69f22` and its
successors, every cell drives its child to write a **worktree-relative** file and asserts its
**placement** — that the bytes landed in `<state>/<project-hash>/agents/<agent-id>/worktree`, which
is the node marion says it placed the child in — and criterion 9 is asserted **unconditionally**
rather than being satisfied by a cell that could not write (`cross_product.rs:40-70, 987-1006`).

**Tests that could pass by failing to look.** Four classes, all closed: leak checks that reported no
survivors when `ps` itself failed (`305c03d`, `4b147c4`, five files); a corrupt contract dropped by a
`filter_map` so "exactly one contract persisted" passed with one good file beside one unreadable one
(`0fff646`); scratch directories that leaked on every failing run, 425 of them and 22 MB, now
removed by an RAII guard whose whole point is the failure path (`cb6ab2e`); and three `spawn`
parameters **declared in the tool schema and silently dropped** — `background`, `isolation`,
`verification` — now refused by name (`77557e3`, design §11 item 23). The last is the same
silent-failure family §12 catalogues in other people's tools, found in marion's own.

**Also pinned, so it cannot regress quietly:** worktree reap is unwired and leaves branch residue,
and a task id is single-use for the life of the repo (`1ff90d9`); auth declaration and the
`--base-url` no-op are asserted per adapter (`c3e73ca`); the contract provably reaches the **root**
on all four wires (`5fcfe75`).

**`--base-url` is no longer a pinned no-op — it is refused (`b4283b4`).** `c3e73ca` pinned the drop
so it could not regress quietly; `b4283b4` removed the drop instead. `resolve_base_url`
(`crates/marion-supervisor/src/bin/marion.rs:238-268`) refuses **two** distinct cases, deliberately
not collapsed, because the remedy is identical and the diagnosis is not. Without `--canned`, a
**loopback** URL is refused as a credential hazard: the node presents the operator's real login and
a loopback endpoint is marion's canned provider or some other local process, so honouring it would
send a real credential to a fake server. Any **other** URL is refused as *not implemented*: every
adapter drops it in that mode (no `ANTHROPIC_BASE_URL`, no `GOOGLE_GEMINI_BASE_URL`, no codex
`model_providers` entry, no opencode provider block), so the run would have reached the vendor
directly while looking like it honoured the gateway. `--canned` is untouched — with it, an explicit
URL, then `$MARION_BASE_URL`, then marion's own endpoint. `(false, None)` is the default and the
premise: no endpoint at all, each harness resolving the vendor it is already logged in to.

**The count, and why it is not the point.** **570** tests, up from 488 on 2026-08-04 and from 138 on
2026-08-03. Read that as evidence-per-claim, not as progress: almost none of it is new capability,
and the largest single contributor is the same assertion applied to sixteen pairings instead of one.
**What 570 was measured on:** `cargo test --workspace` at HEAD `08b4fde`, exit 0, every target
compiling, 30 test binaries. The raw sum of `test result:` lines is **572**; `tests/journal.rs`
re-execs its own binary for the two-process test, so that sum over-reports it by two — it is 6
tests, not 8. It is **five** crates, not the four this paragraph used to say: `marion-testsupport`
is a workspace member and contributes 22 of them. The figure moves with almost every commit; three
landed while this line was being written (`45d53b8`, `7ff470e`, `08b4fde`), which is the reason it
is dated and attributed to a sha rather than left standing as a fact about the project.

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
weekly is the unbounded cost, and fifteen corrections in one day of research is the evidence.

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
