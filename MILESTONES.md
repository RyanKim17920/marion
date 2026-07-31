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

### Embedded VT — scope is NOT yet known

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
| **S1** | Can Claude Code be driven from raw Rust, including **interrupt**? | Claude adapter needs a TS sidecar — **changes the process model** |
| **S2** | Which DECSTBM shape do the harnesses emit; can the chosen VT keep scrollback? | scrollback needs custom history above the scroll region |
| **S3** | Codex `shared` lifecycle — does an idle app-server survive? | heartbeat requirement returns |
| **S4** | Can a `Stop` hook re-prompt a stopping agent (Claude Code, Codex)? | explicit-reporting needs another mechanism |

**S1 runs first** — it is the only open question that changes the language and process model.

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
3. Cross-harness via `Stop` hooks where they exist — **unverified in practice, spike S4** —
   else one more turn on the channel marion already owns.
4. Still nothing → synthesize from the transcript tail, mark `Exited{Unreported}`, surface it
   visibly. **Never silently promote a status message to an answer.**

`Unreported` is a testable state; L3 asserts on it.

---

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
70 s, all for an *explicit* `daemon stop`). The heartbeat requirement is struck. Something did
kill a server once — plausibly started via `codex app-server daemon`/`remote-control`, or a
child of an exiting shell. **Spike S3.** The related inference that this explained a Codex
session "stopping" when opened is also withdrawn.

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
