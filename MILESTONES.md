# marion — Goals & Milestones

A meta-harness: run any agent harness, on any model, as a first-class subagent of any other
harness — with one UI over the whole tree.

## The core contract

```
Agent(harness, model, tools, prompt, …) -> handle
handle: observe · steer · interrupt · result
```

If a milestone doesn't make that primitive better, it isn't a milestone.

## Non-negotiable principles

1. **Agent types are launch specs, not personas.** A type is a declarative record (`harness`,
   `model`, `effort`, `tools`, `isolation`, `prompt`) that *compiles* to a process invocation —
   argv, env, config files, MCP injection. That compile step is the per-harness adapter contract.
2. **Control is config-time, not runtime.** marion controls harnesses by owning their launch
   configuration, never by parsing a pty.
3. **The pty is a display device.** Never a control plane.
4. **ACP is a floor, not a ceiling.** Prefer a harness's native control plane
   (`codex app-server`, Claude stream-json, opencode HTTP+SSE). ACP is the breadth tier.
5. **Capabilities are resolved and rendered, never assumed.** Degrade visibly — grey out what a
   node cannot do rather than failing at runtime. Resolution is two-stage: a static table keyed
   `(harness, version, mode)` from `marion doctor`, then narrowed by handshake where one exists.
   `interactive` and `opaque` have no handshake and use the static table only.
6. **Vendor payloads are carried, never discarded.** Normalize the common case; keep the rest
   for per-harness renderers.
7. **The supervisor outlives the UI.** Killing the TUI must never kill running agents.
8. **Never re-open a session; never stop holding one *while it runs*.** Every harness's
   `resume` starts a *new process* against a single-writer transcript — that is what breaks live
   agents. marion holds each running child's channel from spawn and renders it continuously.
   "Opening" a running subagent is a view switch, never a connection event.
   *Deliberately reaping an **idle** node (§Resource model) is the one sanctioned exception: the
   process is killed but the ownership claim is retained and the node stays resumable.*
9. **Direct-MCP spawn is the primary delegation path.** The parent calls `mcp__marion__spawn`
   and receives a structured tool result. The Claude Code Agent-tool shim is optional sugar —
   it launders results through an extra LLM turn and costs a full ~462 MB process.

## Topology

Star, not mesh: 1→N fan-out and N→1 fan-in. Every edge has a parent. A node may address only
its own descendants and its parent; sibling addressing is denied unless explicitly granted.

---

## Spawn modes

A per-agent-type field alongside `harness` and `model`.

| mode | control | display | semantics | works on |
|---|---|---|---|---|
| **`shared`** | typed **and** a live native TUI simultaneously | real TUI, attachable/detachable | live, structured | **Codex** (`app-server` + `codex --remote`) |
| `headless` | typed (`claude -p --output-format stream-json`, `opencode serve`) | marion's widgets | live, structured | harnesses with a control plane |
| `interactive` | keystroke injection into the pty | real TUI, faithful | tailed transcript, lagged | anything that writes a transcript |
| `opaque` | keystroke injection | real TUI | none | literally anything |

`shared` is preferred wherever it exists — the only mode with no tradeoff. VERIFIED on Codex:
one `codex app-server --listen ws://127.0.0.1:PORT`, threads driven by
`turn/start`/`turn/steer`/`turn/interrupt`, `item/*` events consumed, and a `codex --remote` TUI
attached in a pty only when a human wants to watch. A second independent JSON-RPC client was
confirmed able to steer a thread owned by a live TUI, with the TUI rendering the result.

> **Open:** `shared` mode means multiple marion-owned clients on one app-server thread, which is
> *not* the double-open hazard below (there is exactly one writing process — the app-server).
> The ownership registry tracks the **thread**, not each client connection. Confirmed only for
> marion-owned clients; a foreign client on the same thread is untested.

**Mode is authoritative over capabilities**, not parallel to them. Mode sets the ceiling; caps
may only sit at or below it.

## Session ownership — marion's invariant, because nothing else enforces it

VERIFIED: **neither Codex nor Claude Code locks a session.** Two concurrent `codex resume`
processes on one id both start, both hold the same inode read/write, and neither is refused.
Same for `claude --resume` on a live session.

Writes are `O_APPEND`, so records are not clobbered — the failure is **semantic divergence**.
Both processes continue from the same base state, and Codex rollout records carry `turn_id` but
no parent/branch pointer, so the fork is unreconstructable. Claude Code's `uuid`/`parentUuid`
DAG degrades more gracefully but still branches.

marion therefore keeps a registry of live session ids and refuses to open any it already holds.
Branch explicitly (`codex fork`, `claude --fork-session`). For live Codex threads use
`thread/read` + `turn/start`; **never `thread/resume`** — it is the disk path and fails on
unmaterialized rollouts anyway.

**A reaped-idle node keeps its ownership claim.** The id stays held and resume goes through the
registry, so reaping never becomes an accidental double-open.

**Scope, honestly:** this protects marion from marion. It cannot stop a user opening the same
session in another terminal.

---

## Verified facts

Version-stamped. **Re-verify before relying on anything here** — these tools auto-update and
break things. During design alone, Gemini moved 0.40.1 → 0.53.0 and Codex removed
`wire_api = "chat"` outright.

Baseline: **Claude Code 2.1.220 · Codex CLI 0.145.0 · opencode 1.17.3 · Gemini CLI 0.53.0.**

> **Local Codex moved to 0.146.0 on 2026-07-31** — spike S2's scripted Enter hit Codex's startup
> update prompt, which defaults to "Update now", so `~/.codex/packages/standalone/current` now
> points at 0.146.0. Both versions remain on disk and S2 captured both; behavior was identical for
> every question it tested. This is itself a data point: **the harnesses will update themselves
> out from under marion at a keystroke**, which is exactly what §7.7's binary-path pinning and
> version-stamped capabilities exist for.
Qwen Code and Amp claims below are **unstamped and unverified locally.**

### Launcher requirements

- **`--tools <tools...>` is the allowlist** for Claude Code. Never compile `tools:` to
  `--disallowedTools` — a denylist requires enumerating the complement of the built-in set and
  re-deriving it every release, guaranteeing silent privilege escalation when a tool is added.
- `ANTHROPIC_API_KEY=""` when using `ANTHROPIC_AUTH_TOKEN` — a non-empty key silently wins.
  The empty string is inherited by grandchildren and shelled-out tools; scope it narrowly.
- `CLAUDE_CODE_ATTRIBUTION_HEADER=0` — a per-request nonce destroyed third-party prefix caching
  (0% → 99.7% hit rate when stripped).
- Codex `--listen` needs a literal `IP:PORT` (`SocketAddr`); a hostname is a hard
  `InvalidWebSocketListenUrl` and `wss://` is rejected. `--listen unix://` needs its own
  directory, not a bare path in `/tmp`. `--remote` works only on interactive subcommands.
- Codex reserves provider ids `openai`, `ollama`, `lmstudio`, `amazon-bedrock`.
- Both binaries require a real TTY (`codex`: "stdin is not a terminal"; `claude` falls back to
  demanding `--print`).
- **`CLAUDE_CODE_CHILD_SESSION`:** set `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` on
  `interactive` children rather than scrubbing it. See *Retractions*.
- **⚠ `CLAUDE_CONFIG_DIR` isolation breaks OAuth auth.** The macOS Keychain entry is keyed to the
  *real* config dir, so an isolated child cannot authenticate on a subscription. **Config
  isolation and subscription auth are mutually exclusive for Claude Code.** This promotes the
  fileless path (`--agents`, `--mcp-config`, `--settings`) from "preferred" to **load-bearing**.
  Whether `CODEX_HOME` / `GEMINI_CLI_HOME` share the coupling is **unverified**.
- **⚠ Codex hooks are trust-gated and fail silently** — no warning, no log — until trusted via
  `[hooks.state."<key>"] { enabled, trusted_hash }` in `config.toml`. Key and hash come
  non-interactively from the app-server's `hooks/list`. A required marion setup step.
- **⚠ Branch marion's hook script on `hook_event_name`.** A Stop-shaped `{"decision":"block"}`
  returned from `UserPromptSubmit` blocks the user's prompt outright.
- **Gemini 0.53.0 needs a settings file, not just a key.** `GEMINI_API_KEY` alone now fails with
  `Invalid auth method selected.` marion must write
  `<GEMINI_CLI_HOME>/.gemini/settings.json` = `{"security":{"auth":{"selectedType":"gemini-api-key"}}}`.
  There is no env-var equivalent — settings.json is the only lever. Headless still needs
  `--skip-trust` or `GEMINI_CLI_TRUST_WORKSPACE=true`. **The HTTPS-unless-localhost restriction
  is GONE at 0.53.0** (zero bundle hits; plain-HTTP non-localhost base URLs are accepted), so the
  0.40.1 caveat below is retired.
- **opencode: drive turns with the legacy `POST /session/{id}/prompt_async`.** The v2 path
  `POST /api/session/{id}/prompt` returns 200 with an `admittedSeq` and then **nothing ever
  runs** — no model request is made, messages stay empty — and `POST /api/session/{id}/wait`
  returns `ServiceUnavailableError: Session wait is not available yet`. Broken in 1.17.3 with or
  without the experimental flag.
- **opencode `OPENCODE_EXPERIMENTAL_EVENT_SYSTEM=true`** unlocks the full `session.next.*` stream
  family (`text.started/delta/ended`, `step.started/ended`, `prompt.admitted`, `prompted`).
  Without it only `agent.switched` / `model.switched` appear. The legacy
  `message.updated`/`message.part.updated`/`message.part.delta` family arrives either way and is
  interleaved 1:1 with the new one.

### Keystroke injection (`interactive` / `opaque`)

- **Never send text and `\r` in one write** — both tools run paste-burst heuristics and read it
  as a paste, not a submit. Send text, pause, send `\r` separately.
- **Never wrap in bracketed paste** (`\x1b[200~…\x1b[201~`) — collapses into a multi-line
  attachment placeholder instead of submitting.
- One line at a time; multi-line submit needs correct CSI-u / modifyOtherKeys encodings.
- Wait for boot modals (trust prompt on both; update nag and cwd-selection on Codex resume) —
  detect from screen state, never fire blind.

### Liveness and attach

- `claude agents --json` needs no TTY; returns `pid`, `cwd`, `kind`, `sessionId`, `name`,
  `status` (idle/busy) for interactive *and* background sessions. Best cheap readiness signal.
- `claude attach <id>` is background-jobs-only. Given an interactive id it prints
  `No job matching…` **and exits 0** — never branch on its exit status.
- `claude daemon` is a documented subcommand (`run`, `status`, `logs`, `uninstall`,
  `stop [--any] [--keep-workers]`). marion must not depend on it, but must tolerate
  `claude daemon stop` terminating marion's children — report that as external termination,
  never as normal completion.

### Transcript tailing

Safe: the writer never learns of the reader; records are whole-line, `O_APPEND`, flushed per
record. Caveats:

- Claude Code transcripts are mostly non-conversation (`queue-operation`, `attachment`, `mode`,
  `permission-mode`, `ai-title`, `last-prompt`, `file-history-*`). Filter by `type` and follow
  `parentUuid`; never read linearly.
- Codex keeps a SQLite index at `~/.codex/state_5.sqlite` (`threads(id, rollout_path, …)`). The
  rollout fd is held only while loaded/writing — **fd presence is not a liveness signal.**
- Rollout compression is **not** a live hazard. See *Retractions*.

### Embedded VT — RESOLVED by spike S2 (2026-07-31)

**Verdict: `alacritty_terminal` retains scrollback correctly; scope is as designed.** Full detail
in design doc §5.3. The three things that changed:

1. **Claude Code 2.1.220 uses the alternate screen** — so it has *no* scrollback to retain and is
   viewport-only. Scrollback is a **Codex-only** problem. (The claim below that "neither uses the
   alternate screen" was stale and is corrected.)
2. **Top-offset DECSTBM regions exist but are harmless** — Codex emits them only with reverse
   index (scrolling *down*), which never feeds history. Every history-producing scroll is
   top-anchored. `vt100` still retained **0** lines in every real capture; confirmed unusable.
3. **The real hazard is `CSI 3J`** (erase scrollback), which Codex emits on *every resize* and
   alacritty honors — history goes to zero on every SIGWINCH. **marion intercepts it and keeps
   its own append-only history.**

DECSET 2026 is validated as a reliable frame boundary for TUI test assertions.

### Embedded VT — original notes (superseded above)

VERIFIED: a dumb pty host answering **no** probes runs both TUIs correctly and interactively.
Neither blocks on a reply. `TERM=xterm-256color` suffices; no XTGETTCAP. **Neither uses the
alternate screen.**

Required coverage: SGR (incl. 24-bit), CUP/ED/EL, DECSET 2026 synchronized output, reverse
index (`\x1bM`), DECSTBM scroll regions, OSC 0, OSC 8 (Codex emits many), cursor save/restore.
Optional but cheap: answer DA1 and OSC 11 for real theme detection.

**Unresolved:** whether scrollback survives. `alacritty_terminal` rotates rows into history only
when `region.start == 0`; `vt100` keys on `scroll_top != 0 || scroll_bottom != rows-1`. So with
a bottom-anchored region (`ESC[1;20r`) alacritty keeps history and vt100 does not; with a top
offset (`ESC[2;24r`) **neither does**. Which shape the harnesses emit is **unmeasured** —
**spike S2**. Also `renderable_content()` is viewport-only; scrollback needs negative-`Line`
grid indexing or driving `display_offset`.

---

## Resource model

Measured 2026-07-30 on macOS 26.5.1 / arm64 / **24 GB**. `ps -o rss` overstates ~2x by counting
shared clean pages — use `phys_footprint`, validated against anonymous-page release on kill.

| harness | marginal footprint/instance | startup | idle CPU | fds |
|---|---|---|---|---|
| `codex app-server` | **53 MB** | **40 ms** | 0.20% | 40 |
| `claude` (idle) | 182 MB | 0.8–1.8 s | 0.33% | 25 |
| `claude` (**active session**) | **462 MB** | — | — | — |
| `opencode serve` | 175 MB | 350 ms | **1.15%** | 28 |
| `gemini` (2 procs) | 345 MB | 3.0 s | 0.00% | 97 |

Extrapolated (not directly observed; the test machine was 24 GB) for 1 Claude root + N Codex
children with sessions loaded: **~200 on 32 GB, ~440 on 64 GB**. Two caveats: these were taken
with MCP servers disabled, and they hold **only for the direct-MCP spawn path** — an Agent-tool
shim in front of each child adds a full ~462 MB Claude process (~9x per child).

Memory binds before fds or process limits in every scenario. **Context accumulation, not process
overhead, is the real driver** — an in-use session is ~2.5x a fresh one. macOS compression
absorbs idle fleets well (8 idle Claude instances grew the compressor 473 MB while total
anonymous pages *fell*; zero swapouts).

**Rules:**
- Rasterize only the visible pane; off-screen nodes parse into their grid without rendering.
- Coalesce high-rate output from unwatched agents.
- **Reap idle nodes** — running nodes never. Kill the process, keep the transcript, retain the
  ownership claim, resume transparently on the next message. Record `ReapedIdle` in the journal
  *before* the kill, so a supervisor crash can distinguish a deliberate reap from an orphan.
- **Do not use SIGSTOP as hibernation.** It works cleanly (buffered pty keystrokes replay on
  SIGCONT; opencode answered HTTP in 46 ms after a 175 s stop) but **saves no memory** —
  footprints unchanged, swapouts never incremented. It buys CPU only, and idle Claude already
  costs 0.33% of a core. Sole exception worth the complexity: `opencode serve` busy-polls at
  **1.15%/core while completely idle**.
- Expose a cross-harness concurrency cap.

---

## Any harness × any model — verified viable (2026-07-30)

Model choice is a **separate plane from control**. No harness has a client-side model allowlist,
and **per-process isolation works everywhere**, so two children of the same harness can run
different models simultaneously. Anthropic documents the gateway path and does not prohibit it
(support disclaimer only). The reverse — routing third-party clients through Pro/Max OAuth — is
prohibited and enforced; never do it.

| Harness | Override | Per-child isolation | Proxy must serve | Difficulty |
|---|---|---|---|---|
| opencode | `provider{}` JSON | `OPENCODE_CONFIG_CONTENT` (inline) | **nothing** — speaks all 4 wire formats natively | trivial |
| Claude Code | `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` | `CLAUDE_CONFIG_DIR` / `--settings` / env | Anthropic Messages, `POST /v1/messages?beta=true`, SSE mandatory | easy |
| Qwen Code † | `OPENAI_BASE_URL/_API_KEY/_MODEL` | `QWEN_HOME` | OpenAI Chat Completions | easy |
| Gemini CLI ‡ | `GOOGLE_GEMINI_BASE_URL` | `GEMINI_CLI_HOME` | Gemini `v1beta` generateContent + **mandatory** `:countTokens`, `:embedContent` | medium |
| Codex | `[model_providers.X]` | `CODEX_HOME` / `-c` inline / `-p` profile | **OpenAI Responses ONLY** | hardest |
| Amp † | — | — | — | **blocked** |

† unstamped/unverified locally. ‡ measured at 0.40.1; local is 0.53.0. `GEMINI_CLI_HOME` and
`GOOGLE_GEMINI_BASE_URL` survive at 0.53.0, but the HTTPS-unless-localhost restriction is
**UNVERIFIED** there.

Order of attack for **model injection**: opencode → Claude Code → Qwen → Gemini → Codex.
(Distinct from adapter build order, which is claude-code → codex → acp → opencode.)

**Codex is the hardest.** `wire_api = "chat"` was removed (only `responses` is accepted). It
also sends `include: ["reasoning.encrypted_content"]` unconditionally with no off switch, emits
`apply_patch` solely as a Lark-grammar custom tool, wraps MCP in a proprietary `type:"namespace"`
item, and gates startup on `GET /models` returning `{"models":[…]}` rather than `{"data":[…]}`.
Codex + ChatGPT-subscription auth cannot use a custom `base_url` at all (token lacks
`api.responses.write`).

**Amp is structurally blocked.** BYOK removed; `AMP_URL` is the Amp server, not a model
endpoint, and is hostname-allowlisted; inference runs on Sourcegraph's machines.

**marion does not have to build the proxy** — LiteLLM / Vercel AI Gateway / OpenRouter already
translate. marion owns the launcher primitives. (If LiteLLM is ever used: pin by hash and run
out-of-process; PyPI 1.82.7/1.82.8 shipped credential-stealing malware.)

**Universally expect to lose** prompt-caching fidelity (silently — reports show `usage: 0`, not
errors), reasoning-state round-tripping across multi-turn, and accurate token counts.

---

## Chosen tooling (researched 2026-07-31)

| Area | Pick | Version |
|---|---|---|
| PTY (Unix, async-native) | `pty-process` (`features = ["async"]`; default is `[]`) | 0.5.3 |
| PTY (Windows — deferred) | `portable-pty`, blocking only; needs a thread bridge | 0.9.0 |
| VT emulator | `alacritty_terminal` (+ `vte` `ansi`) | 0.26.0 / 0.15.0 |
| VT differential oracle | `avt` (asciinema's emulator) | 0.18.0 |
| TUI snapshot tests | `ratatui::backend::TestBackend` + `insta` (note `assert_scrollback_lines`) | 0.30.2 / 1.48.0 |
| ACP client | `agent-client-protocol` (repo: `agentclientprotocol/rust-sdk`) | 2.0.0 |
| Reference ACP agent | `agent-client-protocol-test` → `testy` (unpublished; git dep) | 0.11.0 |
| Canned model provider | `wiremock` + port Codex's `core_test_support::responses` SSE builders | 0.6.5 |
| Expect-style CLI tests | `expectrl` | 0.9.0 |
| Fixture format | asciicast v3 NDJSON, hand-rolled serde (the `asciicast` crate died in 2018) | — |
| Seed VT corpus | vendor `vt100-rust` `tests/data/fixtures/` (MIT) | — |

**`wezterm-term` is not on crates.io** — only a third-party fork. Avoid unless willing to carry
a git pin. `alacritty_terminal` is the pick because it is published, handles OSC 8 and DECSET
2026, and is actively released — **not** because it solves scrollback (see S2).

**Prior art to port** (all read-and-port, not dependencies):
- `github.com/openai/codex` → `codex-rs/app-server/tests/common/mock_model_server.rs` —
  `wiremock` + `SeqResponder` + `.expect(n)`; with `codex-rs/core/tests/common/responses.rs`
  (`ev_function_call`, `ev_assistant_message`, `ev_completed`). **This is the canned provider.**
- `github.com/openai/codex` → `codex-rs/app-server-test-client` — a runnable CLI driving
  app-server over WebSocket.
- `github.com/agentclientprotocol/registry` → `.github/workflows/protocol_matrix.py` — probe
  list and status classification for `marion doctor` (supported if status ∈ `{success,
  invalid_params, resource_not_found}`).
- **esctest2** (Python) is the real automated VT conformance suite; pointing it at an embedded
  Rust VT needs a shim binary owning a pty and answering DSR/CPR/DECRQSS. Later milestone.

**No harness ships an official mock mode.** Base-URL redirection is the supported mechanism.

---

## Testing strategy

**E2E through real harnesses is the test.** Only inference is canned. Full detail in the design
doc §8.

- **L1** pure units — spec compilation, IR normalization, journal replay, capability resolution,
  ownership, ordering. Most of the code.
- **L2** fixture replay — recorded real streams replayed into planes, asserting on the IR. Also
  format-drift detection. **Every spike must emit a fixture.**
- **L3** fault injection against **real** harnesses (not fakes — a simulated Codex only tests our
  assumptions about Codex): kill mid-turn, truncate a transcript line, block a socket, leave a
  permission unanswered, attempt a double-open, crash the supervisor mid-spawn. Keep small.
- **L4** *the* E2E layer — real binaries, real pty, real transcripts, real MCP, real subagent
  spawning, driven through marion, with only the model endpoint canned.
- **L4.5** self-hosted TUI driver — marion hosts marion. The same VT emulator and keystroke
  injection built for M3, pointed at marion itself. `TestBackend` + `insta` +
  `assert_scrollback_lines`; DECSET 2026 frame brackets give a precise "assert now" signal. No
  AI in the loop, so it gates commits.
- **L5** live smoke — nightly, real models, structural assertions only, never on text.
- **L6** agent-driven acceptance — against the canned provider so only the tester is stochastic.
  Cross-checks **screen against IR log** and attaches artifacts as evidence, never bare claims.

**`marion doctor`** probes each installed harness and produces the static capability table —
the same code as runtime capability resolution. Must include a **keystroke-injection submit
check**, the most version-fragile mechanism.

**Known limitation:** L1–L4 test marion against harnesses *as recorded*. Drift is caught only on
re-record. `marion doctor` is the smoke detector, not a guarantee.

**Security, before recording any fixture:** fixtures contain system prompts, repo contents, and
anything secret that appeared in tool output. Redaction pass + pre-commit secret scan are
mandatory; prefer recording against the canned provider. Design doc §7.1.

---

## Spikes and milestones

**Spikes answer a question, emit a fixture, and stop.** Procedures in design doc §9.

| # | Question | Fail consequence |
|---|---|---|
| ~~**S1**~~ | ~~Can Claude Code be driven from raw Rust, including **interrupt**?~~ | **RESOLVED 2026-07-31 — PASS.** Pure Rust confirmed; TS-sidecar branch dead. Protocol in design doc §5.2 |
| **S2** | Which DECSTBM shape do the harnesses emit; can the chosen VT keep scrollback? | scrollback needs custom history above the scroll region |
| ~~**S3**~~ | ~~Codex `shared` lifecycle — does an idle app-server survive?~~ | **RESOLVED 2026-07-31.** Yes, always — no reaper exists; the retraction stands. But `THREAD_UNLOADING_DELAY = 1800 s` unloads **unsubscribed** threads. Design doc §5.2 |
| ~~**S4**~~ | ~~Can a `Stop` hook re-prompt a stopping agent?~~ | **RESOLVED 2026-07-31 — PASS on both.** Use `{"decision":"block","reason":…}`, **not** `additionalContext`. Codex hooks are trust-gated and fail silently. Design doc §7.6 |

~~**S1 runs first**~~ — done. S2/S3/S4 remain.

**A fifth spike was added by the Codex review and is now the highest-priority one:**

| # | Question | Outcome |
|---|---|---|
| ~~**S5**~~ | ~~Does a late-joining observer receive the event stream in Codex `shared` mode?~~ | **RESOLVED 2026-07-31 — PASS.** `thread/resume` **is** the subscribe mechanism (there is no `thread/subscribe`); additive, non-disruptive, works mid-turn. Sequence and six caveats in design doc §5.2 |

**Reconciled:** both earlier claims were half-right. `thread/read` genuinely does not subscribe
(the Codex review was correct); `thread/resume` on a *live, loaded* thread is a safe additive
subscribe (our "never resume a live thread" was wrong). The original `no rollout found` failure
was the narrow case of a thread whose rollout had not yet been materialized.

**One policy this forces:** approvals fan out to *all* subscribers as blocking requests, first
answer wins. marion answers approvals only on threads **it originated**; on attached threads it
renders them read-only and lets the owning UI decide. Otherwise marion races a human for their
own permission prompt.

**Milestones.** Acceptance criteria in design doc §9; each is a gate, not a vibe.

- **M1** — one real cross-harness hop over the direct-MCP path. Includes the canned provider,
  since L4 depends on it.
- **M2** — supervisor split: TUI crash does not kill agents; reattach restores the full tree.
- **M3** — tree UI + embedded terminal.
- **M4** — N→1: Codex as root spawning Claude children. Same code path.
- **M5** — ACP breadth with capability-negotiated degradation.

Post-M5: ModelProxy translation across four wire formats.

---

## Explicit reporting — no implicit returns

"The agent's final text is its return value" conflates *done, here is the answer* with *stopped,
waiting on something*. (Observed live during design: a research agent returned "Agents are
researching in parallel. Waiting on results." as its result.)

1. marion's MCP server exposes **`report(result)`**; agent types are prompted to call it.
2. On a stop with no report, marion re-prompts once: *are you reporting, or waiting?*
3. Cross-harness via `Stop` hooks returning `{"decision":"block","reason":"…"}` — **verified
   working on both Claude Code and Codex (S4)**. Not `additionalContext`: invisible in the stream
   on Claude Code, nonexistent on Codex. Guard with `stop_hook_active`. Codex requires a one-time
   hook-trust bootstrap or the hook silently never runs.
4. Still nothing → synthesize from the transcript tail, mark `Exited{Unreported}`, surface it
   visibly. **Never silently promote a status message to an answer.**

`Unreported` is a testable state; L3 asserts on it.

---

## The strategic challenge (independent Codex review, 2026-07-31)

An independent review by Codex/GPT-5.x argued that **marion as scoped is not worth building**.
It is recorded here in full because it is the strongest argument against this project and should
not have to be rediscovered.

**Its case.** The scope is at least five products — a delegation broker, a session manager, a
terminal multiplexer, a cross-vendor observability system, and a model-routing proxy — and most
of the plan is not required to deliver the core contract. ACP already occupies the normalization
layer with ~30 adapters, and vendors are moving *upward* into orchestration natively (Codex has
`spawn_agent`/`wait_agent`/`interrupt_agent`; Claude Code has the Agent tool and agent teams).
"Any harness × any model" is not a moat — it is proxy configuration plus documented losses
(prompt caching, reasoning state, usage accounting) — while expanding the security boundary to
credentials and inference traffic.

**Its sharpest point, which is hard to argue with:** the number of version-sensitive facts
already discovered, and the three retractions in this file, are evidence that marion proposes to
own the union of every harness's compatibility burden — argv, env vars, transcript formats,
terminal behavior, event schemas, approval semantics, session persistence, attach behavior —
before writing a line of code.

**Its recommended wedge:** *an ACP-first local delegation broker for cross-harness worktree
delegation with auditable results — ownership, diff attribution, replayable task contracts.*
Keep launch specs, worktree isolation, single ownership, intent-before-spawn journaling, direct
structured returns, explicit result states, diff/verification capture, and no auto-merge. Cut the
TUI, embedded vendor TUIs, PTY emulation, the mode taxonomy, transcript tailing, native adapters
in v1, mid-turn steering, the universal IR, and the model proxy.

### Assessment — what we accept and what we don't

**Accepted, and already applied:** every technical correction (Lamport → `global_seq`, `Tier` →
`Provenance`, modes as presets over `ExecutionSurfaces`, group-commit instead of fsync-per-record,
the `thread/resume` reconciliation, `turn/completed` as the authoritative terminator, the
server-initiated approval methods, `codex exec --json` for one-shot children). The **task
contract** (design doc §6.7) is a genuine addition we did not have, and it is the right durable
primitive.

**Accepted on sequencing:** prove delegation before building anything that displays it. Our
milestone order already does this (M1 is the cross-harness hop, M3 is the UI), but the *design
doc* over-invested in terminal detail before the core was proven. The corrective is to build a
disposable vertical slice — `spawn` → worktree child → structured contract → cancel, with no
daemon, no VT emulator, no proxy — and only then decide what to keep.

**Not accepted: cutting the TUI and the model plane outright.** Codex is optimizing for the most
defensible product; that is not the same objective. Watching and clicking into running
cross-harness subagents is the *stated purpose* of this project, and any-harness × any-model is
an explicit north-star goal. A headless broker would be more defensible and would not be the
thing we set out to build. The honest resolution is sequencing, not amputation — the UI comes
after the thing it displays, and the model plane stays post-M5 where it already was.

**The risk we are consciously accepting:** owning several unstable integration boundaries at
once. Mitigations already in the design are `marion doctor`, version-stamped claims, pinned
binary paths, and fixture-based drift detection. If those prove insufficient in practice, the
Codex wedge above is the fallback scope — narrow to ACP-first and delete the adapters.

## North star

marion is not ultimately a wrapper. The harness-bridging layer is the foundation, not the
product — long term this is the layer where multi-agent work is defined, executed, observed, and
reasoned about, independent of which vendor's CLI is underneath: its own agent/task model, its
own execution semantics, its own persistent state, its own interface as the primary place work
happens.

Deliberately under-specified. It exists as a decision filter: **prefer choices that keep
marion's own model authoritative (its IR, its registry, its log) over choices that make marion a
faithful mirror of one harness's concepts.** Concretely — do not let Claude Code's Agent
semantics become marion's semantics. Parity with it is right for M1, but parity *via translation
into marion's model*, not by adopting its model wholesale. The two look identical at M1 and
diverge completely by the time you want your own execution semantics.

## Explicitly out of scope (for now)

- **The graph-plan system** sketched in `info.md`: a DAG of plan nodes with executable
  per-node exit criteria, a "node 0" that validates the test infrastructure itself, and a
  red-before-green rule requiring each node's test be observed failing before the work and
  passing after. Good idea, separate product, would consume this one. Build it later as a
  marion consumer.
- Arbitrary agent-to-agent mesh routing.
- Remote hosting. Local-first; remote falls out of the supervisor split later.
- Windows. `pty-process` is Unix-only and `portable-pty` has no async.

---

## Retractions and corrections

Three claims were stated confidently and were wrong. Recorded so they are not rediscovered.

**RETRACTED — Codex app-server idle reaping.** An earlier measurement reported an idle
`codex app-server` SIGTERM'd at ~86–90 s, and a 25 s heartbeat was made a hard requirement with
a thread-loss story attached. **It did not reproduce** — an idle server with zero clients was
alive at 160 s, and `codex-rs/app-server-daemon/` contains no idle reaper at any duration (only
`START_TIMEOUT` 10 s, `OPERATION_LOCK_TIMEOUT` 75 s, `STOP_GRACE_PERIOD` 60 s / `STOP_TIMEOUT`
70 s, all for an *explicit* `daemon stop`). The heartbeat requirement is struck.

**S3 (2026-07-31) confirms the retraction and identifies the likely culprit: our own tooling.**
Six invocations — bare `--listen`, `daemon start`, orphaned, with and without live threads and
held clients — all survived 43 minutes. No timer exists in that band. The probable killer is
`codex-app-server-test-client`'s `kill_listeners_on_same_port`, which runs `lsof -tiTCP:<port>`
and kills whatever answers, with no delay floor — which also explains death while SIGSTOPped.
The related inference that this explained a Codex session "stopping" when opened is withdrawn.

**But S3 found a real hazard in its place: `THREAD_UNLOADING_DELAY = 1800 s`.** `thread/start`
returns a rollout path but does not create the file, and an **unsubscribed** thread is unloaded
after 30 minutes on a healthy server. Read-only probes do not refresh it, and a *connection*
heartbeat cannot help — the timer is on the thread. marion must keep a subscriber attached
(`thread/resume`, per S5), materialize the rollout by running a turn, or be able to re-create the
thread. Also: run a bare `--listen ws://` marion owns, and **never** `daemon bootstrap` /
`remote-control start` — `daemon stop` does not stop the updater, which then restarts the server,
and there is no way to disable it.

**CORRECTED — `CLAUDE_CODE_CHILD_SESSION`.** Earlier text said scrubbing it was mandatory or no
transcript is written. A/B testing at 2.1.220 wrote transcripts **both** ways. The real gate
additionally requires the interactive path, not-being-a-teammate, and no tmux ambient marker, so
`-p`/headless is unaffected; a third variable `CLAUDE_CODE_SKIP_PROMPT_HISTORY` has its own
suppression path. Claude Code also *sets* this variable itself when spawning children, so
scrubbing changes how the child identifies itself.

**CORRECTED — scroll regions and scrollback.** Earlier text claimed vt100 loses scrollback under
DECSTBM and `alacritty_terminal` does not, and used that to disqualify vt100. **Both drop it**,
at different thresholds — see *Embedded VT* above. alacritty remains the pick for other reasons,
but the VT scope question is **open**, not de-risked.

**RETIRED — rollout compression as a tailing hazard.** `codex-rs/rollout/src/compression.rs`
requires `ThreadStoreConfig::Local` *and* a default-off feature flag, enforces
`MIN_ROLLOUT_AGE = 7 days` against mtime, and explicitly skips referenced and fork-pointed
rollouts; reads are transparent and appends re-materialize plain `.jsonl`. **A live transcript
can never be compressed out from under a tailer.** Dropped from the fault-injection list.
