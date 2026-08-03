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
| Gemini CLI | `GOOGLE_GEMINI_BASE_URL` | `GEMINI_CLI_HOME` | Gemini `v1beta` + mandatory `:countTokens`, `:embedContent` | medium |
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
*different* harness's user configuration unless severed. **Why this is [partial]:** opencode has
never been exercised as a marion child end to end, no opencode adapter exists, and several questions
remain **unknown** — whether pacote's git-plugin path honours `ignoreScripts`, whether LSP children
get an explicit kill, whether the `~/.claude/ide` lock scan is gated, whether `--port` on `run` is
truly unconsumed. The three original server-path claims also remain unfixtured, because S13 did not
re-measure them. **S7 answered NO** —
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
- **M2 [open]** — supervisor split: TUI crash does not kill agents; reattach restores the tree.
- **M3 [open]** — tree UI + embedded terminal.
- **M4 [open]** — N→1 fan-in: a Codex root spawning two Claude children concurrently.
- **M5 [open]** — ACP breadth, degrading per resolved capabilities.

Post-M5: ModelProxy translation. Acceptance criteria for each: design doc §9.

### M1 — where it actually stands (2026-08-03)

**The delegation core works.** A real `claude` root calls `mcp__marion__spawn`; a real `codex`
child starts in a worktree, edits a file, and returns through marion's `report` tool over MCP (S6's
primary branch); the root receives the structured contract as a tool result; a deliberate
out-of-scope write is caught detectively; the whole run is canned. `cargo test --test m1_hop`, ~2.2 s
*(measured 2026-08-03; 138 tests across four crates)*. The contract criterion is asserted on the
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
Nothing below exists yet, and none of it is covered by any M1 criterion:

- **The journal (design §4.3) — the significant one. Nothing survives a supervisor restart today.**
  `Orphaned` marking and reap recovery both depend on it, so M2's replay criteria have no substrate
  to stand on.
- The registry.
- **Descendant gating (§7.6) is not in code** — principle 11 above is specified, not enforced.
- `verification` command execution, so a contract's `evidence` is always empty.
- The `Stop` hook path.
- `marion doctor --adapter`, `marion-term`, `marion-tui`, `marion-proto`.
- `status` / `wait` / `list` appear in `--allowedTools` but are **not implemented** — the bridge
  declares only `spawn` and `report`. An allowlist entry for an undeclared tool is inert.

What does exist: `marion run <agent-type> --prompt …` launches the root; contracts persist uncapped
to `<agent-dir>/contracts/<task_id>.json` with the capped copy returned; timeouts are enforced with
a descendant-pgid sweep; scope is checked both preventively (at spawn) and detectively (git-derived).
Mechanism for all of it: design doc §5–§7.

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
