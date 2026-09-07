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

## Where it actually stands — 2026-09-05, at `d914eb7`

**Markers:** M1, M2, M4 `[done]`; M3 `[partial]` on C1's recorded manual session alone; M5
`[partial]` on clause 3 alone (`NodeSummary` names a harness, not an ACP agent's handshake). The
dated audit under "Status convention" names the test behind each.

**Harnesses.** Nine rows in `Harness::ALL` (`marion-core/src/harness.rs`): eight terminal
harnesses — `claude`, `codex`, `gemini`, `opencode`, `copilot`, `goose`, `cline`, `qwen` — as
`HarnessSpec` rows plus measured hooks, and `acp`, one adapter over any ACP agent. Headless: every
one runs as a child and a root through `run_spawn` (`harness_matrix` 8 cells; `cross_product` 57
cells; qwen has a row and no column, see the audit). ACP: `acp:<command>` launches any ACP agent
with no row (`acp_child.rs`), with five refinement rows in `acp::AGENTS` — `opencode`, `gemini`,
`claude-acp`, `codex-acp`, `copilot` — and `marion doctor --acp-command "<cmd>"` probes one by its
own handshake. Pinned versions: claude 2.1.220–2.1.226 and 2.1.261; codex 0.146.0/0.146.1/0.147.0;
gemini 0.53.0; opencode 1.17.3; copilot 1.0.83; goose 1.49.0; cline 3.0.61; qwen 0.23.0.

**Native facade.** `marion <harness> <its own flags>` runs the harness's real TUI through the
relay as a journaled root. Enabled lanes: `claude`, `codex`, `opencode`, `copilot`. Disabled by
name: `gemini` (system-settings merge unmeasured), `goose`, `cline`, `qwen` (interactive shape
unmeasured). Resume: `marion resume <agent-id>` relaunches a lost root under its own id where its
row carries a resume flag and a session was journaled (claude, codex, opencode, copilot, qwen);
gemini, goose, cline and ACP refuse by name. TUI: `marion tree` (44-column tree, detail pane,
`caps:` strip dimming what doctor has not measured, status row) and `marion attach`.

**E2E, and how to run it.** Default `cargo test --workspace` drives real harness binaries against
the canned provider and skips loudly where a binary is absent; the version gate is
`marion_testsupport::PINNED_HARNESSES`. Native facade from the shipped binary:
`cargo test -p marion-supervisor --test native_facade_e2e` (per enabled lane; needs the harness on
`PATH` and a real PTY). Restart and resume: `cargo test -p marion-supervisor --test
restart_resume -- --ignored` (real codex, detached supervisor; since 2026-09-06 it asserts the
resumed child's own turn — a post-relaunch provider request carrying the resume prompt and the
first life's transcript — and its code-0 exit, so a relaunch codex rejects at argv is red in
seconds with the exit quoted; the pre-fix row fails it at exit 2). Live ACP: `--test acp_child --
--ignored` spends Copilot tokens. Cross-harness: `--test harness_matrix`, `--test cross_product`,
`--test journal_wiring`, `--test m4_fan_in`. Panes: `--test pane_attach`.

**Structure.** `sentrux` quality signal 6456 at `b85ea30` (cycles 0, redundancy 9170,
modularity 5057, equality 5441, **depth 4444 — the bottleneck**); the day started at 4856 with
three import cycles. The signal was measured at `b85ea30`; the commits from there to `d914eb7`
are tests and docs only, so it is not re-measured. The depth is the one spawn path — CLI → detach → serve → handler → run →
harness row → grammar — and the design has exactly one such path, so the remaining headroom is
the tool's formula over a chain marion will not shorten; do not chase the number past that.

**Open, honestly:** C1's recording; the four disabled lanes; `SIGTSTP`/`SIGCONT` from the
*shipped* binary (a real kernel stop and continue through the relay is observed by
`native_relay_stop_and_continue_are_observed_by_a_job_control_leader`, commit acb1cc5, but the
`spawn_pty` facade fixture cannot observe it); child resume and a linked
worktree's cwd on resume; Linux `/proc` start identity unmeasured; descendant gating (§7.6) and
`verification` execution still not in code; the ACP agent identity on `NodeSummary`.

---

## Verified harness facts

Version-stamped. **Re-verify before relying on anything here** — these tools auto-update and break
things. In one day of research, Gemini moved 0.40.1 → 0.53.0, Codex 0.145.0 → 0.146.0 (a scripted
Enter hit its update prompt mid-experiment), and Codex removed `wire_api = "chat"` outright.

Baseline: **Claude Code 2.1.220 · opencode 1.17.3 · Gemini CLI 0.53.0 · Codex CLI — 0.145.0 for S3
and the alt-screen/`/diff` capture, 0.146.0 for S5 · GitHub Copilot CLI 1.0.83 (S24, 2026-09-05) ·
goose 1.49.0 (S26, 2026-09-05) · Cline CLI 3.0.61 (S27, 2026-09-05) · Qwen Code 0.23.0 (S25,
2026-09-05)**.
(The 0.146.0 captures contain no `?1049h` at all; the alt-screen evidence is the 0.145.0 file.)
Amp claims are **unstamped and unverified locally**.

**Harness support is registry data, not per-harness code paths (2026-09-05).** Each harness is a
`HarnessSpec` row in `crates/marion-harness/src/<harness>.rs` — program, argv and env as closed
`Arg`/`Env` shapes, a `StreamGrammar` for its output, its tool names, its spelling of marion's
tools, its MCP declaration route per auth mode, its audit constraint, and its resume flag — rendered
by one `spec::render` and read by one `grammar` engine. The adapter keeps only what no table can
hold: the measured refusals (claude's argv prompt, gemini's missing model, copilot's BYOK inputs),
the model rules, the derived base URLs, the document emitters, and ACP's agent binding. A sixth
harness is a row plus those hooks — and goose, cline and qwen landed as exactly that on 2026-09-05:
one row each, an `axes`/`fields`/`config_files` hook or two, three new closed shapes
(`ToolSpelling::ServerDoubleUnderscoreTool`, `LiveDeclaration::ArgvInline`, `Arg::Isolation`), and
no other per-harness branch. qwen's row reads `claude_code::STREAM` outright, because its stream
**is** Claude Code's (S25).
`adapter::tests::spec_render_matches_the_adapter_that_measured_it`
and `every_committed_fixture_reads_the_same_through_the_grammar` pin every row to the hand-written
adapter it replaced, byte for byte on every committed fixture. **Resume flags, measured on the
installed binaries' `--help` and carried as row data** (plan step 5): claude `--resume <id>`, codex
`exec -C <cwd> resume <id>` (S29, 0.147.0: `exec resume` has no `-C`, and the demo's relaunch died
at exit 2 with `unexpected argument '-C'` until the row put `-C` ahead of the subcommand; pinned by
`codex::tests::a_resume_places_the_working_root_ahead_of_the_subcommand_that_does_not_take_it`),
opencode `run --session <id>`, copilot `--resume=<id>`, qwen `--resume <id>`
(the id off `init.session_id`, replayed under the same `QWEN_HOME` and cwd, S25); gemini's
`--resume` takes `latest` or an index rather than a session id, goose's `--resume --session-id <id>`
takes an id no frame of its stream carries (S26), and cline's `--id <id>` forces interactive mode
and exits 1 headless (S27), so those three rows say `None` and a resume on them is refused by name;
ACP's row says `None` too, and its resume is the protocol's `session/load`, built by
`AcpAdapter::session_declaration` with the same `mcpServers` block and sent by the driver only to an
agent whose `initialize` advertised `loadSession` (2026-09-05). **Wired 2026-09-05:**
`LaunchSpec::resume: Option<String>` is seeded into `Fields::resume` by the one neutral seeding, so
`compile`/`compile_pane` render the row's grammar or refuse by name with no per-harness branch
(`a_harness_whose_row_refuses_resume_is_refused_by_name`, RED with `no field named resume`).

**A version here names the binary a behaviour was measured on, not the binary that will run.** The
installed `claude` on this machine is **2.1.261**, not the 2.1.220 stamped above and throughout this
file. `5792140` replaced the exact pin with a **set** — `marion_testsupport::PINNED_HARNESSES`, the
one table every version check in the workspace reads — whose **entry zero never moves**, because
entry zero is what the prose claims, and whose tail carries versions since observed green with the
evidence beside each. claude's set is `["2.1.220", "2.1.222", "2.1.223", "2.1.224", "2.1.225",
"2.1.226", "2.1.261"]` and codex's is
`["0.146.0", "0.146.1", "0.147.0"]` — both auto-updated mid-session on 2026-08-06 and codex again
on 2026-08-07, §7.7's hazard arriving live, and each new version was admitted only after its probes
were re-run and compared against the committed captures field for field; gemini and opencode each
pin exactly one. So
read "2.1.220" as *"the version the turn-one `\"tools\":[]` shape and the
`tests/fixtures/s9` `can_use_tool` frame were captured from"*, and read a green suite as *"and
2.1.222 through 2.1.226 and 2.1.261 were checked against them too."*

**2.1.261, admitted 2026-09-05 at `0b2b4f1`, after a thirty-five-release gap.** The installed
binary went 2.1.226 → 2.1.261 without any intermediate version being run here, and none of the
pinned versions is still on disk (`~/.local/share/claude/versions/` holds 2.1.251, 2.1.252,
2.1.259 and 2.1.261), so there was no one-axis A/B against a pinned build. What was re-run, all
green with only the table entry widened: `harness_matrix` (5), `cross_product` (26), `m1_hop` (1),
`m4_fan_in` (1), `acp_child` (2), `permission_round_trip` (8), `journal_wiring` (18),
`depth_gate` (4), `timeout_kill` (1), `worktree_reap` (8), `no_git` (5), `restart_resume` (1,
`--ignored`), the six native facade suites (`native_facade_smoke`, `native_injection_binding`,
`native_facade_cli`, `native_facade_gate`, `native_mode_matrix`, `native_bootstrap`; 22 tests),
and the workspace `--lib` set (1437). `acp_child`, `permission_round_trip` and `journal_wiring` each failed once
first with marion's MCP bridge never becoming ready (`McpNeverReady(30s)`, "the bridge never
answered tools/list") under a 20–28 load average from concurrent builds, and passed in full on an
isolated rerun minutes later — environmental, kept here so the next reader does not mistake it
for drift. **`pane_attach`'s claude cell was red** at its "a keystroke arrives" step, and the
first reading — that the `\r` was lost in marion's pane input-admission path — was wrong. Traced
byte by byte with temporary instrumentation: `marion attach` read `0x0d` from fd 0, sent
`node/pane-write` with `"DQ=="`, the supervisor admitted it and `write_all` on the node's master
returned `Ok`. Nothing was dropped. Three things made the cell red, none of them marion's input
path. **(1) A stale oracle.** The step waited for an `i` record in the node's `pty.cast`, and
pane-v1 keyboard input writes none by design — `PtyHost::write_opaque_input_admitted` records
length evidence only, so raw keystrokes are never persisted; that assertion could only time out.
**(2) Harness drift, measured on a bare pty with no marion in the path:** claude 2.1.261 paints
its trust dialog **twice** — the first frame is replaced by a fresh copy within a few hundred
milliseconds — and a key applied to the first copy is undone by the second: an arrow-down that
moved `❯` to "Yes" was followed, with nothing further typed, by a frame with `❯` back on "No,
exit", and a `\r` typed the instant "trust" appeared never took effect while a `\r` typed later
always did. The fixture typed the instant the dialog was on screen, so every run typed into the
copy that was about to be thrown away. **(3) The cursor default moved:**
2.1.261's dialog reads *"Quick safety check: Is this a project you created or one you trust?"*
with the cursor on **"No, exit"**, where 2.1.226 defaulted to accepting, so a bare Enter exits
claude (measured: exit status 1). The fixture now drives the dialog with arrow-down as its own
probe — re-sent only after the screen has been still, and judged only once the screen has stopped
moving, so the second paint is seen before Enter is typed — then Enter through the same probe, and
every "a keystroke arrived" assertion reads the screen (the composer, the prompt marker, the typed
marker) rather than the recording. The delivery leg the
`i` oracle stood in for is pinned where it is observable:
`handler.rs::a_negotiated_clients_first_opaque_keystroke_reaches_the_child` drives the real socket
into a real child and was the first test to assert an *admitted* opaque write is delivered (every
prior opaque-input test was a refusal). The s10/s11/s14/s16 probes were **not** re-run; those
readings remain 2.1.226's.

**How fast this goes stale, now that there is a rate rather than an anecdote.** On 2026-08-06 two
of the four pinned harnesses auto-updated underneath a single session — codex 0.146.0 → 0.146.1 in
the morning, claude 2.1.222 → 2.1.223 in the evening — and in the four days of S1–S17 the four
binaries moved seven times between them. **Assume a pinned version has days of life, not months**,
and read a red pin as the ordinary weather rather than as an incident. The maintainer's move is
fixed and is not "widen the set": re-run the probes that a green suite would *not* re-run, compare
against the committed captures, and add the version with what was compared beside it. The gap that
makes this necessary is that only `tests/fixtures/s9` and codex's `s6`/`s7` shapes are asserted by
a test at all — `s10`, `s11`, `s14` and `s16` are prose (`child_stream`, `child_events` and `node_attach` drove
real harnesses without going through the gate until 2026-09-06, when
`common::script::require_claude_and_codex` put them behind it) — so a green suite is always the
weaker half of the evidence. Two conveniences make the re-measurement cheap and are worth
preserving: every spike keeps a runnable probe under `spikes/`, and both CLIs keep old releases
side by side under a repointed symlink, so **the previous version is still on disk and one axis can
be varied at a time**. That is how s11's `system/hook_progress` divergence was told apart from a
protocol change — and, on 2026-08-07, how 2.1.223's supposedly widened SIGKILL grace was found to
be machine load after all: re-run on the same machine an hour apart, 2.1.223 gave 347–549 ms and
2.1.224 gave 363–458 ms, so the 477–539 ms attributed to 2.1.223 was never a version property.
**Two runs one minute apart are enough to separate versions but not enough to separate a version
from the machine** — where the reading is a duration rather than a shape, take the spread on both
versions before attributing the difference to either.

**The gate chooses the binary now, not only the verdict (2026-09-06).** Overnight three of the
eight pinned harnesses auto-updated at once — claude 2.1.261 → 2.1.263, codex 0.147.0 → 0.153.4,
opencode 1.17.3 → 1.18.29 — and every suite that gates on `on_path` went red for a reason that was
not marion's. Two of the three installers had left the admitted release on disk beside the new one;
nothing put it first on `PATH`. So the suite now resolves pinned harnesses through a **shim**
(`marion_testsupport::Shim`, `crates/marion-testsupport/src/shim.rs`): `.cargo/config.toml`
installs `scripts/cargo-runner.sh` as the cargo `runner`, which starts every test binary with an
empty per-process directory first on `PATH` (`MARION_HARNESS_SHIM`, removed when the process
ends); the first `on_path(<harness>)` in that process lays a symlink there to the **newest
admitted release still on disk** — claude's under `~/.local/share/claude/versions/<ver>`, codex's
under `~/.codex/packages/standalone/releases/<ver>-<target>/bin/codex`, an npm harness's
(copilot, gemini, qwen, cline) under `<marion state>/harness-pins/<program>/<ver>`, installed with
`npm install -g --prefix` only when the binary first on `PATH` is not admitted — and then probes
`--version` *through* the shim. Because the runner's directory is the process's own `PATH`
prefix, every spawn the test causes (`run_spawn` in-process, a `marion` binary, the supervisor it
detaches, the native facade client's `vars_os()`) resolves the same binary the gate passed. A
harness with no per-version store (opencode, goose — Homebrew formulae) or none of whose admitted
releases is on disk falls through to `PATH`, and the gate refuses with its usual diagnosis plus
*"no pinned release on disk"* and where it looked; drift stays loud, only the choice of binary is
taken back from the auto-updater. Proved in `shim::tests` with a fake versions store and a fake
newer `--version` script first on `PATH` — the shim picks the pinned release, and an absent one
falls through to the refusal — and live: `harness_matrix` 7/8 with claude resolved to 2.1.261 and
codex to 0.147.0 through the shim, the opencode cell refused by name because 1.17.3 is no longer
on disk. **The admission ritual is a script:** `scripts/admit-harness.sh <harness> <version>`
widens that harness's `accepted` entry (entry zero untouched), runs `marion-testsupport --lib` and
every suite under `crates/marion-supervisor/tests` that names the harness and gates on `on_path(`
— sequentially, each bounded by `MARION_ADMIT_BOUND` (default 1800 s) with the whole process group
killed at the bound — restores the table on any red, and on all green writes the dated observation
beside the entry and prints this section's paragraph and the commit message; it never commits. So
drift is handled in four parts, each with one owner: the **gate** refuses an unadmitted version by
name; the **shim** runs the admitted release while it is still on disk; the **ritual** admits a new
version only with the suites re-run beside it; and at runtime the shipped binary trusts none of
this — `marion doctor --capabilities` measures the installed harness and the `caps:` strip dims
what it has not measured, which is how marion itself adapts to a version the suite has never seen.
The pin is a property of the evidence, not of the product.

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

**GitHub Copilot CLI runs as a marion child at $0.00 — the fifth binary, on the opencode cell's
wire.** *(copilot 1.0.83, macOS darwin 25.5.0, measured 2026-09-05 against the canned provider;
fixture `tests/fixtures/s24/`; witnessed end to end by `harness_matrix`'s fifth cell, which starts
marion's own bridge from the document `CopilotAdapter` writes and reads the child's `report` back
out of the persisted contract.)* Installed with `npm i -g @github/copilot@1.0.83` — the 0.0.367 that
npm had left on `PATH` has no BYOK, no `--output-format json` and no `--acp`, so nothing below was
measurable before it. The facts the adapter rests on, every one of which a naive port from the
other four would get wrong:

- **BYOK is env, not config, and it needs an explicit model.** `COPILOT_PROVIDER_BASE_URL` selects
  a provider outright (no GitHub login), takes marion's `…/v1` form verbatim and appends
  `/chat/completions`; `COPILOT_PROVIDER_TYPE=openai` + `COPILOT_PROVIDER_WIRE_API=completions` is
  the canned provider's already-proven wire. Without `--model` the CLI exits 1 before any request
  (`BYOK providers require an explicit model`), so the `copilot` built-in carries
  `COPILOT_DEFAULT_MODEL` and the adapter refuses that default under `--live`, as opencode does.
- **Both of §3.1's axes exist, as two flags with two spellings of one tool.** `--available-tools`
  is what the model *sees* (everything unnamed is withheld and listed in a `session.info` frame);
  `--allow-tool` is what *runs* without a prompt. The MCP tool is `marion-report` on the first
  axis — a hyphen, the fifth spelling — and `marion(report)` on the second; the file tools are
  `create`/`edit`/`view` on the first and the *kind* `write` on the second. `--allow-tool=create`
  and `--allow-tool=marion-report` both grant **nothing**. Hence `writes_without_a_declaration`
  is `false` here (as on Claude Code and gemini), `read → view` needs no grant, `write → create`
  needs `write`, and the contract records `allow-tool:<pattern>` — prefixed, because copilot's
  `write` kind is spelled exactly like marion's own verb.
- **⚠ In `-p` mode an ungranted call is denied and the run exits 0.** `Permission denied and could
  not request permission from user`, `code: "denied"`, the model's closing text, `exitCode: 0`. A
  `report` the bridge answered `isError: true` also exits 0. §12's silent-success shape twice
  over; the stream is the only place either is described, so `CopilotAdapter::parse_stream` reads
  the verdict off `tool.execution_complete` (paired to `tool.execution_start` by `toolCallId`) and
  the exit code decides nothing on its own. A provider 500 is five retries over 30 s, then
  `session.error` and `exitCode: 1`.
- **The CLI holds turn one for its MCP servers itself.** `session.mcp_server_status_changed`
  `pending → connected`, `session.mcp_servers_loaded`, then `user.message`, on every capture; the
  ceiling is 60 s, after which it proceeds tool-less and says `status: "failed"` in that frame. So
  the gate §6.1 step 8 makes marion build for Claude Code is built in, and no readiness marker is
  compiled — `mcp_servers_loaded` is what a caller should assert instead.
- **Isolation is `COPILOT_HOME`, and `HOME` must be left alone.** The former relocates config,
  session state, logs, the sqlite store and `mcp-config.json` (`--additional-mcp-config @file`
  *augments* it for one session); the latter is where the 1.0.83 launcher finds its platform
  package (`~/Library/Caches/copilot/pkg/`). `COPILOT_OFFLINE=true` closes GitHub auth, telemetry,
  web tools, the GitHub MCP server and auto-update in one switch, and rides with the provider block
  (dropped under `--live`); `COPILOT_AUTO_UPDATE=false` is kept under both.
- **The API key is redacted from the CLI's own output.** Every occurrence of
  `COPILOT_PROVIDER_API_KEY`'s value in the JSONL becomes `******` (a key `fake` printed a model
  `fake-model` as `******-model`). marion's placeholder appears in nothing a child prints; a live
  key is the operator's and is never placed by marion. Do not choose a canned key that is a
  substring of a tool name, a model, or anything a narrative might say.
- **`copilot --version` prints `GitHub Copilot CLI 1.0.83.`** — the version ends a sentence, and
  the pin parser had to learn to strip one trailing dot to identify the binary at all.

**A copilot root works too, and that took one generalisation.** `cross_product` grew copilot's
column and row — nine cells, `q`–`y`: each of the four roots spawning a `copilot-impl` child that
writes through `create` and reports, and a `copilot` root spawning each of the five — and seven of
the nine were driven green on this machine (the two that pair with claude are gated behind claude's
version pin, which 2.1.261 does not satisfy). What made the root cells possible: `root::ROOT_ALLOWED_TOOLS`
was four `mcp__marion__*` strings — Claude Code's spelling compiled into every root — and copilot
is the first `LaunchOnly` adapter that reads `allowed_tools` into a flag, where that spelling names
nothing and the root launches with none of marion's verbs, at exit 0. It is now `ROOT_VERBS`, marion's
own vocabulary, spelled per harness by `HarnessAdapter::marion_tool_name` exactly as `run_spawn`
already spells a child's `report`; a claude root's `--allowedTools` is byte-identical to before.

~~Not measured, and so not claimed: `--acp` (it starts; no session has been driven, so `acp::AGENTS`
has no row)~~ **Measured 2026-09-05 (S28, `tests/fixtures/s28/`):** `copilot --acp` opens a session
and calls marion's tool as **`marion-report`**, but only when the bridge is declared on its own
`--additional-mcp-config`; `session/new`'s `mcpServers` is accepted and ignored on 1.0.83. It is the
`copilot` row of `acp::AGENTS` (`acp::Declaration::Argv`), and a real copilot child ran through
`run_spawn` on 2026-09-05 (`tests/acp_child.rs`, `#[ignore]`d for spend). Still not measured, and
so not claimed: `--resume`/`--session-id`, and the `journal_wiring`, `depth_gate` and
`launch_only_root` matrices, whose matches name `copilot` as *not yet* a cell rather than omitting
it.

**goose runs as a marion child and as a marion root at $0.00 — the sixth binary, on the opencode
cell's wire, as a row plus two hooks.** *(goose 1.49.0 via `brew install block-goose-cli`, macOS
darwin 25.5.0, measured 2026-09-05 against the canned provider; fixture `tests/fixtures/s26/`;
witnessed end to end by `harness_matrix`'s sixth cell and by `cross_product`'s goose column and
row — eleven cells, `za`–`zk`, all green on this machine — which start marion's own bridge from the
one `--with-extension` token `GooseAdapter` renders and read the child's `report` back out of the
persisted contract.)* No install was needed; the bottle present was the latest. The facts the row
rests on:

- **Env alone selects the provider; there is no configure step.** `GOOSE_PROVIDER=openai`,
  `GOOSE_MODEL`, `OPENAI_API_KEY` and `OPENAI_HOST` with no `config.yaml` anywhere ran first time.
  The host is taken **verbatim** and `OPENAI_BASE_PATH` (default `v1/chat/completions`) appended,
  so marion hands its `…/v1` form and `OPENAI_BASE_PATH=chat/completions`. goose probes
  `GET …/v1/models` first; the canned server's `text/plain` answer does not stop it
  (`goose-models-plain.*`). `GOOSE_MODEL` has no default, so the `goose` built-in carries
  `GOOSE_DEFAULT_MODEL` and the adapter refuses that default under `--live`, as copilot does.
- **The declaration is one argv token, and goose persists it — so the node token does not ride
  it.** `--with-extension "marion:ENV=v … <command> <args>"` is split on whitespace; every `ENV=v`
  pair is stored **verbatim** in the sqlite session store (`goose session list --format json`
  prints them back; a `--no-session` run still touches the WAL). The token carries the bridge's
  identity (`MARION_REPO`, `MARION_AGENT_ID`, …) and the capability token travels on the process
  environment, which the extension child **inherits** (`goose-env-inherit.mcp.jsonl`). A bridge
  path with whitespace is refused by name (`HarnessError::Unspellable`). The model sees
  `marion__report` — `<extension>__<tool>`, the sixth spelling of one tool — and `tools/call`
  carries the bare `report` with `_meta.agent-tool-call-request-id` = the provider's call id.
- **`--no-profile` is the only way to load nothing but marion.** With an empty config dir and no
  flag goose loads developer, apps, extensionmanager, todo and the skill/delegate tools — nineteen
  tools, and it wrote `~/.local/share/goose/apps/clock.html` doing so. `--with-builtin developer`
  adds `edit shell write tree read_image`, **unprefixed and as one unit**: there is no per-tool
  grant, so `write` compiles the whole extension, `read` is refused (the extension has no
  read-only tool), `writes_without_a_declaration` is `false`, and the contract records
  `with-builtin:developer` or `with-builtin:none`.
- **⚠ Neither fault is an exit code.** An `isError: true` MCP result arrives as
  `toolResult.status: "success"` with `value.isError: true`, the model gets another turn, exit 0 —
  the row reads `isError`, never `status`. A provider 500 is four retries over ~7 s, then an
  ordinary assistant `message` beginning `Ran into this error:` and a zero-token `complete`, exit
  **0**: the stream has no error frame type at all, so a provider fault reads as the report that
  never came, and nothing else.
- **Headless approval is `auto` or nothing.** `GOOSE_MODE` unset behaves as `auto`; `approve`
  aborts at exit 1 after the `toolRequest` frame with `Tool approval required in non-interactive
  mode`; `chat` withholds every call and feeds the model a canned "skipped" result that the stream
  cannot tell from success. marion states `GOOSE_MODE=auto` under both auth modes.
- **Stream grammar is two frame types and no session id.** `message` frames whose `content[]`
  items are `toolRequest` (`id`, `toolCall.value.{name,arguments}`) and `toolResponse` (same `id`,
  `toolResult.value.{content,isError}`), then `complete`. No frame carries the session id — not
  with `--no-session`, not with `-n`: it is in the `-q`-suppressed banner, the `agent-session-id`
  request header and `goose session list`. `--resume -n <name>` / `--resume --session-id <id>`
  exist and were driven, but with nothing to hand back the row's `resume` is `None`.
- **`GOOSE_CONFIG_DIR` does nothing on 1.49.0; `HOME` is the isolation.** `goose info` reports
  `~/.config/goose` with it set and a `config.yaml` placed there is ignored; `HOME` relocates
  config, the session store and the `llm_request.N.jsonl` request dumps together. `-q` is required:
  without it a three-line banner precedes the JSONL on stdout. `goose --version` prints ` 1.49.0`
  with a leading space and no name.

Not measured, and so not claimed: a 404 or empty list on the `/v1/models` probe, keychain access,
`--with-streamable-http-extension`, the interactive TUI's reading of `--with-extension` (the native
lane ships disabled), and the `journal_wiring`, `depth_gate` and `launch_only_root` matrices, whose
matches name `goose` as *not yet* a cell.

**Cline CLI runs as a marion child and root at $0.00 — the seventh binary, on the opencode cell's
wire, and the one whose remembered surface no longer exists.** *(cline 3.0.61 via `npm i -g cline`,
macOS darwin 25.5.0, measured 2026-09-05 against the canned provider; fixture `tests/fixtures/s27/`;
witnessed end to end by `harness_matrix`'s seventh cell and `cross_product`'s cline column and row
— thirteen cells, `zl`–`zx`, all green on this machine.)* The binary present was the latest; no
install was needed. The facts the row rests on:

- **3.0.61 is not the Cline the plan remembered.** No `task` subcommand, no `-y`, no `cline-core`
  gRPC host. Headless is the bare positional prompt under `--json`, act mode with
  `--auto-approve` **on by default** (`false` declines every call inside cline — `requires approval
  in a TTY session` — at exit 0), `-c <cwd>`, `--config <dir>`, `--data-dir <dir>`, `-m <model>`.
- **The provider is a document, and a misplaced one is a silent vendor fallback.**
  `<CLINE_DATA_DIR>/settings/providers.json` — byte-for-byte what `cline auth openai-compatible
  -k … -m … -b …` writes — selects `openai-compatible` with key, model and base URL (marion's `…/v1`
  form verbatim; the CLI appends `/chat/completions`). A `providers.json` anywhere else, including
  at `CLINE_PROVIDER_SETTINGS_PATH`, is **not read**: cline writes a default selecting its own
  `cline` provider and POSTs to `api.cline.bot` with no credential, 401, exit 1, nothing at marion's
  endpoint. `-m` is measured to **win** over the document's model (and cline rewrites the document
  with it), which is a live node's channel for one; the `cline` built-in carries
  `CLINE_DEFAULT_MODEL` and the adapter refuses that default under `--live`.
- **⚠ Isolation is three variables *and* two flags, together.** `CLINE_DIR` + `CLINE_DATA_DIR` +
  `HOME` relocate config, state and the rules/skills roots; `--config`/`--data-dir` are what set
  `sandbox` and stop the CLI **forking a hub daemon that outlives the run** (`bin/.cline
  --cline-hub-daemon --port 25463`, alive ten minutes later, a lock file with its pid and auth
  token; `cline doctor fix` or `kill -TERM` clears it). Flags alone still write
  `$HOME/.cline/data/db/sessions.db`. Only the combination left `home/` empty, `pgrep` empty and
  `~/.cline` untouched — hence `Arg::Isolation`, a canned-only argv flag, so a live node never
  points them at marion's directory.
- **The MCP document is honoured at `CLINE_MCP_SETTINGS_PATH`, outside the data dir** — the shape
  `cline mcp install marion --yes --transport stdio -- <cmd> <args>` writes
  (`mcpServers.marion.transport.{type, command, args}`), resolved independently of the data dir, so
  it is the injection route in both auth modes. The model sees `marion__report` (goose's spelling)
  and the wire carries the bare `report`; the handshake is protocol `2024-11-05`.
- **The built-in tool set cannot be narrowed.** 26 `type: "function"` tools — `editor`,
  `run_commands`, `read_files`, `spawn_agent` and nineteen `team_*` — are offered whatever
  `global-settings.json` says and whatever flag is passed; `-s` replaces the system prompt, not
  `tools[]`. So `writes_without_a_declaration` is `true`, the record is
  `harness-default:unconstrained`, and there is one `cline` type in both roles.
- **⚠ Neither fault is an exit code on its own.** `content_end.output.isError: true` is forwarded
  to the model, `run_result.finishReason` stays `completed`, exit 0. A provider 500 is three tries
  over ~6 s, then `agent_event error {recoverable: false}`, `run_result finishReason: "error"`,
  exit 1. stderr carries AI SDK deprecation warnings on **every** run.
- **The session id is in no frame, and headless resume does not exist.** `hook_event` carries
  `agentId`/`taskId` (not it); the id is a `data/sessions/<id>/` directory name and a field of
  `cline history --json`. `--id <id>` sets the startup target to interactive before the prompt is
  read and exits 1 under both `--json` and plain output. `cline --version` prints a bare `3.0.61`.

Not measured, and so not claimed: a cwd-local `.cline/` MCP file, `run_result.model.provider` as a
compile-time check, the interactive TUI's reading of `CLINE_MCP_SETTINGS_PATH` (the native lane
ships disabled), and the `journal_wiring`, `depth_gate` and `launch_only_root` matrices.

**Qwen Code runs as a marion child and root at $0.00 — the eighth binary, and a Gemini CLI fork
whose headless surface is Claude Code's.** *(qwen 0.23.0 via `npm i -g @qwen-code/qwen-code`, macOS
darwin 25.5.0, measured 2026-09-05 against the canned provider; fixture `tests/fixtures/s25/`;
witnessed end to end by `harness_matrix`'s eighth cell and `cross_product`'s qwen **row** — seven
cells, `zzf`–`zzl`, a qwen root over each of the other seven harnesses, all green on this machine. There
is no qwen column: 0.23.0's `write_file` refuses a relative path (`File path must be absolute`), the
worktree a child writes into does not exist until `spawn` creates it, and a canned script that
cannot name the path cannot script the write every child cell asserts — so the child side rests on
the `harness_matrix` cell alone, and the matrix says so rather than faking a cell.)* The binary
present was the latest; no install was needed. The facts the row rests on, and the reason the row is
nothing like gemini's:

- **Env alone selects the provider; no login.** `OPENAI_BASE_URL` (marion's `…/v1` verbatim; the
  CLI appends `/chat/completions`), `OPENAI_API_KEY`, `OPENAI_MODEL`; without them a single
  `result` frame, `error_during_execution`, `No auth type is selected`, exit 1. `OPENAI_MODEL` has
  no default, so the `qwen` built-in carries `QWEN_DEFAULT_MODEL` and the adapter refuses it under
  `--live`.
- **The stream is Claude Code's, not Gemini CLI's.** `system`/`init` (`session_id`, `tools[]`,
  `mcp_servers[]`, `permission_mode`), `assistant` `tool_use {id, name, input}`, `user`
  `tool_result {tool_use_id, is_error, content}`, `result {subtype, is_error, permission_denials[]}`;
  the spelling is `mcp__marion__report`; `--resume <session_id>` replays the history and repeats
  the id, keyed by `QWEN_HOME` **and** cwd. The row reads `claude_code::STREAM` and says so, and
  `each_adapter_recognises_a_marion_call_in_its_own_stream_and_no_others` names the pair as the
  one legitimate overlap.
- **⚠ MCP tools are deferred behind `tool_search` by default.** Discovery runs in the background;
  anything that connects after the first declaration list is built is announced in a
  `<system-reminder>` and reachable only through `tool_search` for the whole session — `init`
  lists `mcp__marion__report` while `tools[]` does not. `QWEN_CODE_LEGACY_MCP_BLOCKING=1` makes
  discovery block startup and the tool is in `tools[]` on request one; the row carries it under
  both auth modes.
- **⚠ Three axes of tool restriction, and an empty one is silently none.** Default `--yolo` offers
  28 built-ins. `--core-tools <names…>` is a legacy allowlist with **twelve exempt survivors**
  (`agent enter_worktree exit_worktree get_goal list_agents record_artifact report_findings
  send_message skill task_stop tool_search update_goal`); `--exclude-tools` naming those twelve on
  top yields exactly the declared list; `--bare` ignores `--core-tools`. And `--core-tools` with
  **no names** starts fine and offers every built-in but the excluded twelve — no error, no
  warning — so the adapter refuses a launch that names no tool, and the doctor's probe became
  child-shaped (marion's `report` verb in each harness's spelling) to stay a launch a real spawn
  builds. The record is the list itself, `core-tools:<name>`.
- **Without `--yolo`, headless mode asks a model.** `permission_mode: "auto"` sends the provider a
  classifier request (one tool, `respond_in_schema`) per call and, unanswered in schema, declines
  it with `is_error: true` at exit 0, the denial in `result.permission_denials[]`. `--yolo` runs
  everything; `QWEN_CODE_SUPPRESS_YOLO_WARNING=1` silences its one stderr line.
- **Two declaration routes, as codex has.** A canned node's `$QWEN_HOME/settings.json` carries
  `mcpServers` beside `memory.enableManagedAutoMemory: false` — without that switch an env-only run
  makes a hidden second request, a "managed memory extraction subagent" offered `write_file`,
  `edit`, `run_shell_command`. A live node's `~/.qwen/settings.json` is the operator's, so its
  declaration rides `--mcp-config '{"mcpServers": …}'` inline on argv, measured without `--bare`.
  `QWEN_HOME` relocates settings, sessions (`projects/<cwd-slug>/chats/`), usage and memories, and
  qwen rewrites the settings document it is given (`"$version": 4`); `HOME` still contributes
  `~/.agents/skills` to the prompt and is left alone.
- **⚠ A dead provider is retried 28 times over 86 s and then reported as success.** Seven outer
  attempts of four inner tries, backoff to 27 s, then `assistant` text `[API Error: 500 …]` and
  `result.is_error: false`, exit 0. An MCP `isError` is a `tool_result` with `is_error: true`
  whose content embeds the MCP response, `result.subtype` still `success`, exit 0. `qwen` is a
  launcher (`spawnSync` of `node --expose-gc cli.js`): kill the process group, not the PID.
  `qwen --version` prints a bare `0.23.0`.

Not measured, and so not claimed: the file-path form of `--mcp-config`, a bare positional prompt in
place of the deprecated `-p`, the interactive TUI's reading of `--mcp-config` (the native lane ships
disabled), and the `journal_wiring`, `depth_gate` and `launch_only_root` matrices.

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
| Qwen Code | `OPENAI_BASE_URL` (verbatim) + `OPENAI_API_KEY` + `OPENAI_MODEL` — env only, **no login**; `QWEN_CODE_LEGACY_MCP_BLOCKING=1` or the MCP tool is deferred behind `tool_search` | `QWEN_HOME` (settings, sessions, usage, memories; `HOME` still lends `~/.agents/skills` to the prompt) | OpenAI Chat Completions; a `respond_in_schema` classifier call per tool unless `--yolo`; a hidden memory turn unless `memory.enableManagedAutoMemory: false` | easy — **verified 2026-09-05 on 0.23.0, `harness_matrix`'s eighth cell and `cross_product` `zzf`–`zzl`** |
| Cline CLI | `<CLINE_DATA_DIR>/settings/providers.json` (the file `cline auth openai-compatible` writes; **nowhere else is read** — a misplaced file falls back to the vendor) + `-m` | `CLINE_DIR` + `CLINE_DATA_DIR` + `HOME` **and** `--config`/`--data-dir` (env alone forks a hub daemon that outlives the run; flags alone write `~/.cline/data/db/sessions.db`) | OpenAI Chat Completions | easy — **verified 2026-09-05 on 3.0.61, `harness_matrix`'s seventh cell and `cross_product` `zl`–`zx`** |
| Gemini CLI | `GOOGLE_GEMINI_BASE_URL` | `GEMINI_CLI_HOME` | Gemini `v1beta` + mandatory `:countTokens`, `:embedContent` | medium — **but blocked vendor-side on a personal login; see below** |
| Codex | `[model_providers.X]` | `CODEX_HOME` / `-c` inline / `-p` profile (isolating `CODEX_HOME` **also** breaks auth — but copying `auth.json` in fully restores it) | **OpenAI Responses ONLY** | hardest |
| Copilot CLI | `COPILOT_PROVIDER_BASE_URL` + `COPILOT_PROVIDER_API_KEY` + `--model` (BYOK; **no GitHub login needed**) | `COPILOT_HOME` (never `HOME` — the launcher's package cache lives under it) | OpenAI Chat Completions by default; Responses via `COPILOT_PROVIDER_WIRE_API=responses`, Anthropic Messages via `COPILOT_PROVIDER_TYPE=anthropic` — **the one harness that can be pointed at three of the four wires** | easy — **verified 2026-09-05 on 1.0.83, `harness_matrix`'s fifth cell** |
| goose | `GOOSE_PROVIDER=openai` + `GOOSE_MODEL` + `OPENAI_API_KEY` + `OPENAI_HOST` (verbatim) + `OPENAI_BASE_PATH=chat/completions` — env only, **no configure step, no login** | `HOME` (`GOOSE_CONFIG_DIR` is a no-op on 1.49.0; even `--no-session` runs write the session store and request dumps under it) | OpenAI Chat Completions, plus a harmless `GET …/v1/models` probe | easy — **verified 2026-09-05 on 1.49.0, `harness_matrix`'s sixth cell and `cross_product` `za`–`zk`** |
| Amp † | — | — | — | **blocked** |

† unverified locally.

Order of attack for model injection: opencode → Claude Code → Copilot → goose → Qwen → Cline →
Gemini → Codex.
(Distinct from adapter build order: claude-code → codex → acp → opencode → copilot.)

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
| PTY (Unix, blocking thread-per-host) | hand-rolled `posix_openpt`/`ioctl` implementation in `marion-supervisor::pty` | macOS + Linux |
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

*(Checked against `Cargo.lock` at `b85ea30`, 2026-09-05: `alacritty_terminal`, `vte`,
`ratatui`, `insta`, `globset` and the dev-only `vt100` are dependencies. `agent-client-protocol`,
`testy`, `avt`, `wiremock`, `expectrl` and `portable-pty` are **picks, not dependencies** — the
ACP driver is hand-rolled in `marion-supervisor/src/acp_child.rs`, the canned provider is the
`marion-provider` crate, and CLI E2E tests drive real PTYs through `marion-supervisor::pty`. The
table stays as the record of what was chosen and why; this note is what is actually linked.)*

`wezterm-term` is **not on crates.io**. `vt100` is disqualified on retention, and that figure is
now **derived** from the committed `tests/fixtures/s2` captures by
`crates/marion-term/tests/vt100_comparison.rs` (design doc §11 item 10, closed 2026-08-07): vt100
retains **0** rows where alacritty retains 94 and 16, and **26** where alacritty retains 121 — so
"0 in every real capture" was slightly too strong, and the gap is not. The cause is asserted too:
vt100 drops history under a top-anchored but not full-height DECSTBM, the shape Codex sets on
every frame. `vt100` is a dev-dependency of `marion-term` only — evidence, not a component. The
choice also rests on `wezterm-term` being unpublished and alacritty modelling scrollback at all.

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

**Marker audit against the tree at `b85ea30`, 2026-09-05.** Every marker below was re-checked
criterion by criterion against the code and the test targets that compile at HEAD
(`cargo test --workspace --no-run`: 48 integration targets plus 11 unit targets, exit 0). No
marker moves on this audit; what moved is the evidence under two of them and the list of what is
honestly still open. Per milestone:

- **M1 [done]** — unchanged. `m1_hop` compiles and runs against the canned provider.
- **M2 [done]** — unchanged. Criteria 1–4 are `client_run.rs`'s four named tests; criterion 3's
  "no untracked live process" half is `spawned_barrier.rs`. **Beyond the criteria, a restart is
  now recoverable rather than merely accounted for:** `node/resume` (`Method::ALL` is 16;
  `handler.rs::resume_relaunches_an_orphan_into_its_own_node_id_through_agent_spawns_path`) and
  `marion resume` (`attach_verb.rs::resume_is_dispatched_and_starts_a_supervisor_when_none_serves`)
  land a lost root back under its own id, measured end to end by
  `restart_resume.rs::a_node_resumes_into_the_same_id_after_its_supervisor_is_sigkilled_and_a_new_client_hears_its_next_turn`
  (`#[ignore]`, real codex, run with `--ignored`). Open, stated: Linux `/proc/<pid>/stat` start
  identity is written and unmeasured (macOS only is measured); a linked worktree's cwd is not
  recovered on resume; goose and cline carry no session id in any frame and copilot's arrives
  only on its terminal frame, so a resume on those rows is refused by name
  (`adapter.rs::a_harness_whose_row_refuses_resume_is_refused_by_name`).
- **M3 [partial]** — unchanged, on C1's recorded 10-minute manual session alone. C2 met
  (`pane_attach.rs::a_real_codex_tui_keeps_its_scrollback_across_a_resize_in_a_marion_pane`); C3
  met (`.githooks/pre-commit` over `l45_driver` and `l45_tree`). Every automatable clause of C1 is
  asserted (`pane_attach.rs::a_real_claude_runs_in_a_pane…`), and the native facade the session
  is to be recorded through is live from the shipped binary for four lanes —
  `native_facade_e2e.rs::every_enabled_native_lane_runs_its_real_tui_through_the_shipped_facade`
  and `…::native_facade_sigterm_restores_the_operator_terminal_and_exits_by_signal` over
  `claude`, `codex`, `opencode`, `copilot`. Open, stated: the recording itself; the `gemini` lane
  (disabled: system-settings merge granularity unmeasured, `native_facade.rs`); `goose`, `cline`
  and `qwen` lanes (disabled: interactive shape unmeasured); `SIGTSTP`/`SIGCONT` through the
  *facade* E2E fixture, still not asserted (its `spawn_pty` client is an orphaned process group by
  construction, so the kernel discards the stop). Closed after the audit, same day: a real kernel
  stop and continue through the relay is observed by
  `native_relay::tests::native_relay_stop_and_continue_are_observed_by_a_job_control_leader`
  (`acb1cc5`, a same-session foreground job under a leader that owns the PTY; 20/20 under the
  parallel filter), and the parallel-run flake of
  `native_relay_sigterm_restores_terminal_and_redelivers_to_itself` is fixed at `dcd907d` (the
  fixture stopped draining the master while the exiting session leader still had output to drain).
- **M4 [done]** — unchanged
  (`m4_fan_in.rs::a_real_codex_root_runs_two_real_claude_children_concurrently_and_receives_both_contracts`).
- **M5 [partial]** — unchanged, on clause 3 alone. Clause 1 met (`acp_child.rs`: the canned
  `opencode acp` child, the row-less `acp:python3 …` fake agent through the generic path, and the
  live `copilot --acp` child, `#[ignore]`); clause 2 met
  (`doctor.rs::an_acp_command_gets_its_own_doctor_row_keyed_on_its_own_handshake`). Clause 3 —
  the UI greying *their* capabilities — is not met for the one reason the ruling below gives and
  which is still true at HEAD: `NodeSummary` (`marion-proto/src/model.rs`) carries `harness` and
  `harness_version` and no field for an ACP agent's own `initialize` identity, so a tree node can
  be greyed only at the protocol's static ceiling, never at the agent's. The greying mechanism
  itself is built and measured (`tree.rs::the_trees_greying_is_doctors_own_answer_at_every_key`).
- **Spikes** — S8, S9, S12, S13 stay `[partial]` for the reasons given; S24–S29 (copilot, qwen,
  goose, cline, ACP agents, codex `exec resume` argv) are measured with fixtures under
  `tests/fixtures/s24..s29/`. One
  reading in that set has no Rust assertion behind it and is fixture-only: qwen 0.23.0's
  `write_file` refusing a relative path (`tests/fixtures/s25/`), which is why `cross_product` has
  a qwen row and no qwen column.

**README status reconciliation in this commit moves no milestone marker.** The README now mirrors
the existing M1/M2/M4 `[done]` and M3/M5 `[partial]` markers and removes stale M1 work instructions
and debts that this ledger already records as paid. This documentation-only correction adds no
acceptance evidence, so every marker remains unchanged.

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
    **Corrected 2026-09-05:** the marking was covered, but `node/attach` answered an `Orphaned`
    node with `ResubscribeFrom` — a live-channel verdict from a supervisor that holds no channel
    for it, exactly the answer `registry.rs`'s module doc says §7.3.3 cannot survive being wrong
    about. `handler::attach_mode` now answers `ReplayResumable` for every non-`Live` reap state;
    `an_orphaned_node_attaches_as_replay_resumable_and_never_as_a_live_channel` is the test, and it
    failed on the old arm with `answered ResubscribeFrom` before the fix.
    **Linux is not shipped**: `/proc/<pid>/stat` field 22 is designed and unmeasured, so non-macOS
    returns an explicit refusal that resolves to cannot-tell, and the consequence is tested.
    **This bullet was true of the audit and false of the system until 2026-08-07 (`cf1cd01`), and
    the correction is recorded rather than folded in.** `procid::audit`'s scope is
    `node.pid.is_some()`, so the criterion is only as good as the guarantee that a live node has a
    pid on the record — and that guarantee did not exist. `Spawned` went through
    `journal::record`, which prints the failure to stderr and returns `()`. A barrier that did not
    land (full disk, revoked state directory, short write, a record the 16 KiB cap refuses) left the
    child path calling `observer.started`, the root path setting `spawned = true`, and `agent/spawn`
    answering **successfully** — a live process replay carried no pid for, and therefore one the
    audit could not see at all rather than one it reported. That is §11 item 30's untracked live
    process reached through the very record that is supposed to exclude it, and the criterion's test
    could not fail on it, because a node absent from the walk is absent from the verdict. The
    barrier is now fallible at both spawn paths: on failure the process group is killed, the driver
    reaps it, the owner is never told, and the call returns `UnaccountableNode`. `SpawnIntent` alone
    now means what §6.1 step 7 always claimed it meant. Measured by
    `crates/marion-supervisor/tests/spawned_barrier.rs::a_root_whose_spawned_barrier_fails_is_unwound_rather_than_answered_as_started`,
    with a control beside it, no test seam (an over-cap `--model` is a real encode-time refusal on
    the production path) and a stub that forks a descendant outliving it, so the leak is a process
    the sweep names rather than an inference. Mutation — put the append back on `journal::record` —
    kills it by assertion in 1.2 s, and the leak assertion alone kills it in 3.2 s.
    **A second hole of the same shape was closed with it** (`fda7ca8`), latent rather than live:
    `spawn_pty` announced the pid before wrapping the `Child`, so a panicking `on_started` — or a
    caller that panicked or gave up before `PtyHost::adopt` — dropped the only handle to a live
    process, and `std::process::Child`'s `Drop` neither kills nor reaps. It is unreachable from
    §9's fleet today because both spawn paths refuse `LaunchPath::Terminal` before any process
    exists, so it is not a caveat on this criterion; it is named here because it is the same defect
    class and the next increment makes it reachable.
    **What remains, stated so the criterion is not read wider than it is.** The window between
    `command.spawn()` returning and the barrier landing — one `write(2)` plus one fsync — is
    irreducible, because the pid does not exist before the spawn; a supervisor SIGKILLed inside it
    still leaves a process no record names. And a node's own tool-call descendants are never in the
    journal, so *"untracked"* here continues to mean §11 item 30's sense — a **node** nothing will
    attend to — and not any live process on the machine.
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
- **M3 [partial]** — tree UI + embedded terminal. **Re-audited against the code 2026-08-08 after
  C2 was closed, and again 2026-09-05 at `b85ea30` (marker audit above).** Two of §9's three
  criteria are met; the third is not, and what is missing from it is one thing only and cannot be
  automated. It stays off `[done]` for that one reason.

  **Native facade activation, state as of 2026-09-05 (details in the dated paragraphs below).**
  *Now true:* the shipped `marion <harness>` runs a real native TUI through the relay for every
  enabled lane — `claude`, `codex`, `opencode`, `copilot` — and `tests/native_facade_e2e.rs`
  proves per lane, from the shipped binary on a controlling PTY: a harness cell on the operator's
  grid through `marion_term`; an opaque keystroke (output grows, no `i` record); a resize `r`
  record; `^]d` exits 0 with termios at baseline and the node still in the tree; `SIGTERM` ends the
  client by signal 15 with termios at baseline and the node untouched; a confirmed `KillTree`
  ends the node with `Exited{signal 9}`. The relay's terminal writer blocks on writability instead
  of dying on a full pty queue, a bug the matrix found on its first run. The `opencode` and
  `copilot` lanes were enabled on measurement of their TUIs' `/mcp` views. Since 2026-09-06 the
  facade from a cold project starts the supervisor itself through `detach::ensure_supervisor` —
  the one spawn path `marion run` and `marion resume` already take — instead of refusing when
  nothing serves the project (the demo found it refusing); `native_facade_smoke.rs` now begins
  with nothing serving and reads the identity file the started supervisor published. Same day,
  the native root runs where the operator invoked the facade: the bootstrap wire carries the
  client's working directory (hashed, budgeted, refused unless it resolves to this project's §2
  key and lies outside it) and the production factory runs the vendor there, telling the bridge
  the enclosing working tree — the demo had every harness print `.git` as its workspace because
  the node ran in the canonical project, which is the git common dir
  (`tests/native_bootstrap.rs::the_native_command_runs_in_the_clients_working_directory_not_the_common_dir`;
  `native_facade_smoke.rs` reads the real codex's `pwd -P` through a recording shim over a
  git-initialised project).
  *Still open:* C1's
  recorded 10-minute manual session (through `marion claude`); the `gemini` lane (system-settings
  merge unmeasured); `SIGTSTP`/`SIGCONT` through the shipped facade, not observable from the
  `spawn_pty` fixture (orphaned process group) — a real kernel stop and continue through the relay
  is observed by the job-control fixture in `native_relay.rs` (2026-09-05, `acb1cc5`,
  `native_relay_stop_and_continue_are_observed_by_a_job_control_leader`), but not yet from the
  shipped binary; and single-use re-attach plus default-action restoration, proved at the bootstrap and
  unit boundaries but not re-observable end to end. The parallel-run flake of
  `native_relay_sigterm_restores_terminal_and_redelivers_to_itself` is fixed at its root
  (2026-09-05, `dcd907d`: the PTY fixture stopped reading the master while the exiting session
  leader still had output to drain); 20/20 green under the parallel filter.
  - **Criterion 1 (a real `claude` TUI in a marion pane) — NOT MET, and the only thing left is the
    recorded session.** Every clause a test can reach is now reached; the criterion's own words
    end with *"over a recorded 10-minute manual session"*, and a human has to sit at a screen for
    that. It is not deferred because it is hard, it is deferred because nothing else can perform
    it. The runbook is below.

    Clause by clause, and where each is asserted:

    | clause | covered | by |
    | --- | --- | --- |
    | a real `claude` in a marion-owned pane | yes | `pane_attach.rs::a_real_claude_runs_in_a_pane_…` — a real binary, a real pty, two real `marion attach` clients, hard-failing rather than skipping when `claude` is absent |
    | pre-alt-screen trust dialog on the main screen | yes | same test: the dialog text is read off the **operator's** emulated grid, and the node's recorded output up to that point must contain no `?1049h` — the second half was added here, since "the text is visible" alone would pass for a dialog drawn *after* a switch |
    | alt-screen switch handled | yes, split | live: marion's grid must be on whichever screen the node's own bytes put it on, taking the switch when told and inventing none when not. Positive direction: `marion-term/tests/replay.rs::the_alt_screen_switch_is_handled_including_the_restore_that_never_arrives`, over the committed 2.1.220 captures that switch at bytes 67 and 1900 — one restores at 5866, the other **never does**, which is the absent-restore case. Both directions asserted, plus marion's own restore firing when the node's never comes |
    | resize clean | yes | same live test, and it was already there: the node's cast carries an `r` record with the operator's geometry, at least two geometries exist so a pane born at the operator's size cannot pass, and the operator's screen still renders the node at the **new** width afterwards |
    | mouse through | yes, split | live: an SGR-1006 press **and** release written to the operator's pty master arrive intact in the node's `i` record. The enabling leg — putting the operator's terminal into the modes the node asked for — is `attach.rs::the_nodes_mouse_modes_are_mirrored_onto_the_operators_terminal`, over the mode sequences the 2.1.220 capture carries at bytes 98–122 |
    | permission prompt correct | **no**, and the reading is stated — *corrected 2026-08-08, see below* | this is the *harness's own* in-TUI dialog, not marion's `permission/request` queue — §11 item 22's queue is the duplex `can_use_tool` path, and `compile_pane` deliberately omits `--permission-prompt-tool stdio` so the ask stays in the pane. That omission is asserted (`marion-harness::the_pane_argv_is_a_tui_with_the_headless_shape_isolation`, and now also `…::a_paned_node_compiles_its_grant_on_both_axes_and_names_no_permission_prompt_tool`). The dialog itself needs a paned node **granted a tool it must ask about**, and marion cannot compile one — not because a pane withholds tools, but because it grants them on **both** axes at once |
    | over a recorded 10-minute manual session | **no** | nothing can stand in for it |

    **Correction, 2026-08-08: "a pane compiles `--tools \"\"`" was false, and it was load-bearing.**
    It stood in this table, in `pane_attach.rs`'s header and in the runbook's step 5, each time as
    a property of *panes*. `claude_code::compile_pane` passes `spec.tools` straight through
    (`crates/marion-harness/src/claude_code.rs:215-231`) and the axis is computed by
    `root::availability_axis` **before** the `spec.pane` branch is reached
    (`root.rs:684`, `root.rs:781-784`). The empty axis those readings recorded belongs to the
    **`claude` agent type**, which declares no tool; `claude-impl` declares `[read, write]`, and
    `--pane` is parsed independently of the type name, so
    `marion run claude-impl --pane --repo <a git worktree>` compiles a real grant into a TUI.

    **But that does not hand C1 its dialog, and the reason is sharper than the old one.** §3.1's
    two axes come from **one** declaration — `ClaudeCodeAdapter::permission_axis` unions the same
    native names into `--allowedTools`, deliberately, because opening availability alone is §11
    item 24's measured dead end. `--allowedTools` is *"tool names to allow"* (2.1.226 `--help`),
    i.e. pre-approval. So the paned `claude-impl` above compiles
    `--tools Read,Write --allowedTools mcp__marion__…,Read,Write`: every tool it has is one it is
    already allowed to use, and **the state a permission dialog needs — available and not
    already allowed — is one no `marion run` can reach today.** Pinned, so that a later edit that
    changes it announces itself:
    `marion-harness::a_paned_node_compiles_its_grant_on_both_axes_and_names_no_permission_prompt_tool`.
    That test also fails if someone "fixes" the pane by adding `--permission-prompt-tool stdio`,
    which would convert an answerable question into an item 22 queue entry awaiting a UI that does
    not exist.

    So the clause is **provokable by the human doing the session, not met on its own**, and the
    runbook below now says what to try and what to record if nothing asks. What it is *not* is
    blocked on the grant decision: `1c82946` read it that way, and `root::availability_axis`
    settled that question in the other direction for every root — containment was overruled by the
    operator, and the surviving rule is *no audit, no grant*.

    **The honest cost of the pane, recorded rather than discovered later.** Because the dialog is
    the harness's own, **marion has no record of which permission decisions were made.**
    `PermissionDenied` never fires on this path and `permission_denials` is empty for every paned
    node; §6.7's change record shows what was *written*, never what was asked or what was refused.
    An operator reading a paned root's contract cannot tell a run that was never asked anything
    from one that was asked and said no. That is the price of §11 item 22's absence here, and it is
    the right price only as long as the alternative is a queue with no answerer.

    **A finding that came out of closing this, and it matters beyond M3: §5.3's Claude Code
    terminal readings are stale.** Probed 2026-08-08 against **2.1.225**, in a marion pane and
    again bare, at 100x30 through a boot, a trust dialog, a submitted turn and a resize: **zero
    `?1049h`** and **zero mouse modes**, with only `?1004`, `?2004`, `?2026` and `?2031` present.
    §5.3 says *"Claude Code 2.1.220 uses the alternate screen for its entire session"* and that
    *"both Claude captures enable `?1006`"* — true of 2.1.220, and the committed captures still
    carry it, but no longer true of the installed binary. That is why both clauses above are split
    rather than asserted live: a live assertion in either direction would pin a harness version
    instead of marion. **Nothing was re-recorded and no capture was changed.**

    **And `claude` auto-updated to 2.1.226 mid-session**, which `PINNED_HARNESSES` does not accept.
    Everything above was verified against **2.1.225** — the newest pinned version, still on disk
    under `~/.local/share/claude/versions/` — through a `PATH` shim. 2.1.226 has **not** been
    admitted: that is the ritual `9b0a06d` performs, and doing it as a side effect of this work
    would be exactly the silent pass the table exists to prevent. Until someone runs it, a bare
    `cargo test --workspace` on this machine is red at the version gate. *(`6813810` has since run
    that ritual: 2.1.226 is admitted, and a bare `cargo test --workspace` is green again. The
    paragraph stands as the reading it was — and as the reason the runbook says to check the
    version before starting, since this is the **third** time §5.3's Claude readings have gone
    stale under a session that did not ask.)*
  - **Criterion 2 (a real `codex` TUI in a pane, scrollback across a resize) — MET**, by
    `crates/marion-supervisor/tests/pane_attach.rs::a_real_codex_tui_keeps_its_scrollback_across_a_resize_in_a_marion_pane`.

    What was missing was never the mechanism. `marion-term/tests/replay.rs::scrollback_survives_codex_resize`
    has pinned the `CSI 3J` interception over a committed asciicast all along — 16 history rows
    with suppression on against 1 with it off — but that is `marion_term::Term` fed a recording,
    with no process, no pty and no pane, and **a codex pane could not be launched at all**:
    `CodexAdapter` overrode neither `pane_surfaces` nor `compile_pane`, so `marion run codex --pane`
    was refused with `NoPaneSurface` before anything opened a pty. Both now exist, and the TUI is a
    second compile rather than a flag on `compile_exec` — `exec`'s `--json`,
    `--skip-git-repo-check`, `--output-schema` and `--output-last-message` are argv the interactive
    command rejects.

    The test is a real codex on a real pty that `marion run codex --pane` opened, a real
    `marion attach` rendering it, and `TIOCSWINSZ` on the **operator's** terminal travelling to the
    one master — which is what makes codex emit the `ESC[3J`. Three guards stop it passing
    vacuously: it insists on **20 retained rows before the resize** (driven by `/status` panels,
    because a codex that is merely open wrote 2.3 MB at 100x30 and scrolled *zero* rows); it
    asserts an `ESC[3J` lands **after** the resize record; and it replays the same bytes with the
    interception off, where those rows are gone. Removing `suppress_erase_saved` kills it in 7 s
    with a sentence naming the row counts.

    **Read off the node's own `pty.cast` through `marion_tui::grid_options()`, and stated in the
    test.** The grid that holds the scrollback is the *client's*; `marion attach` paints only its
    viewport and has no scroll key, so no screen the operator's pty could record ever shows a
    history row. An assertion on the operator's screen would assert nothing about scrollback.

    Measured on codex 0.147.0 while closing this: the argv prompt is **submitted** rather than
    seeded (unlike claude's pane), the default is already inline so `--no-alt-screen` is
    deliberately not passed, and no mouse mode is enabled.

    **Attach reliability, 2026-09-05: the codex cell had gone red, and the cause was marion's.**
    `marion attach` exited with *"painting the pane on the operator's terminal: Resource
    temporarily unavailable (os error 35)"* on codex's ~2.3 MB opening frame, and on claude's trust
    dialog under load. `ce346c0` bounded the keyboard read by setting `O_NONBLOCK` on fd 0 and
    `edc366d` carried that into the attach — but in a terminal fds 0, 1 and 2 are one open file
    description and the flag lives on the description, so stdout went nonblocking too and a paint
    larger than the pty's output buffer failed instead of waiting. Reproduced causally rather than
    by re-running codex: `marion-tui::guard::tests::watching_the_keyboard_leaves_a_large_write_on_the_same_terminal_blocking`
    (a dup of one pty slave, a 512 KiB write, nothing draining for 200 ms) and
    `attach.rs::a_paint_larger_than_the_pty_buffer_completes_while_the_keyboard_is_watched` (the
    same through `Session`, a 200x200 frame of distinct colours), both red with that exact error
    before the change. The keyboard is now read with `poll(2)` under the same bound
    (`marion_tui::guard::Keyboard`), in both the attach's reader thread and the tree's
    single-threaded loop; nothing mutates the shared description, so nothing needs restoring on
    drop or panic and the paint blocks until the terminal drains. The codex cell is green again on
    the same binary.
  - **Criterion 3 (L4.5 snapshot tests pass and gate commits) — MET, and now over two targets.**
    `.githooks/pre-commit` runs `cargo test -p marion-term --test l45_driver` **and**
    `cargo test -p marion-supervisor --test l45_tree` unconditionally: no opt-out, no staged-file
    filter, and it reads each pass count back and blocks if it is under that target's minimum, so a
    filter that matched nothing or a renamed target cannot no-op the gate. `core.hooksPath` is
    `.githooks`. Each is latched from inside itself —
    `the_gate_names_this_target_and_only_this_target` for the driver and
    `the_gate_names_this_target_too` for the tree — asserting the hook exists, is executable, names
    that target, and that the declared minimum equals the count of tests in the file. Soft edges,
    stated because they are not visible from the green: the hook is per-clone (`git config
    core.hooksPath .githooks` must be run once), `--no-verify` bypasses it as it bypasses any
    pre-commit hook, and it does not pin `INSTA_UPDATE`, so an environment that rewrites snapshots
    would let the gate pass on rewritten ones.

    *(The second target arrived 2026-08-08 with the tree UI, and it is in `marion-supervisor`
    rather than `marion-tui` for a reason worth keeping: what is worth snapshotting about a tree
    pane is **which actions are greyed**, and `marion-tui` cannot decide that — it has no
    dependency that could reach a capability table. A snapshot taken there would be a snapshot of
    availability bits a test invented. The payload therefore carries a per-style-run census of the
    action strip and a per-row style of the tree column, because the greyed and offered labels are
    the same characters by design and a text-only snapshot would render a completely mis-greyed bar
    identically to a correct one.)*

  - **The tree UI landed 2026-08-08, and it moves none of M3's three criteria.** Said plainly
    because the temptation runs the other way: M3's **title** is *"tree UI + embedded terminal"*,
    and there is now a tree UI. But a title is not a criterion, and **no C1/C2/C3 bullet mentions a
    tree** — C1 is a claude pane, C2 is a codex pane's scrollback, C3 is the L4.5 gate. C3's
    *description* changed above, because the gate grew a target; C3's *status* did not, because it
    was already MET. **M3 stays `[partial]`, blocked on exactly what it was blocked on before: C1's
    recorded 10-minute manual session, which needs a person.**

    **Correction, 2026-09-05, covering the dated run of paragraphs from here down to "The M3
    native facade matrix runs".** Each of them says some form of *production-dark*, *the shipped
    CLI still refuses the native facade relay*, *the production descriptor slice is empty*, *no
    vendor lane is reachable*, or *`run` is unreachable from the shipped CLI*. Every one was true
    on the day it was written and every one is false at `b85ea30`: `PRODUCTION_NATIVE_FACADES`
    carries a descriptor for all eight terminal harnesses, four lanes are enabled, `main` relays
    through `facade_cli::relay_native_facade`, and `tests/native_facade_e2e.rs` runs the shipped
    `marion <harness>` for each enabled lane. They are kept as the record of how the activation
    was built, not as a description of the tree; the block at the head of this milestone is.

    **Implementation reconciliation, 2026-08-12 — no milestone marker moves.**
    Commits `1bc8874`, `e9563c8`, `558a716`, `fd94284`, `d4ecd92`, `a3d86f7`,
    `8698e51`, and `edc366d` add the production-dark durable stream format and the
    committed pane-v1 foundation: bounded retained replay, response-first Ready
    negotiation, completed-pane history, exact ordered End delivery, and client-side
    lossless adoption. Commits `634dabf` and `ffba595` add native launch authority and
    an authenticated, same-connection pane claim. The causal evidence includes
    `pane_replay_delivers_end_retained_during_the_ready_handoff`,
    `pane_stream_capability_reserves_response_first_replay`,
    `pane_v1_consumes_binary_output_dense_resize_and_end_in_order`,
    `pending_native_reservation_gates_both_attach_modes_until_atomic_ticket_claim`,
    and `unacknowledged_native_claim_releases_lease_and_preserves_ticket_for_retry`.

    These are foundations, not native-runtime completion. Production serving still
    installs a disabled native bootstrap service, and the shipped CLI still refuses
    the native facade relay. The command-execution/transparent-relay composition,
    terminal and signal restoration through real exit paths, visible input-failure
    semantics, and a real authenticated native-command E2E remain activation work.
    The current uncommitted runtime/stream implementation is not milestone evidence.
    M3 therefore remains `[partial]`; C1's recorded manual session remains outstanding.

    **Versioned-session hardening in this commit moves no milestone marker.** The production-dark
    `pty::stream` module now writes privacy-preserving v3 sessions, reads committed v2 history,
    validates typed terminal outcomes, bounds and repairs recovery, and pins one private writer to
    the intended stream path. Its evidence is the 45 focused `pty::stream::tests` plus a clean
    `marion-supervisor` library check. At this commit boundary HEAD `pty.rs` has no `stream::*`
    production callsite, so none of that storage is yet emitted or consumed by the runtime. It
    therefore hardens the foundation without satisfying M3 C1 or the remaining native-runtime
    activation work; M3 stays `[partial]`.

    **The `b6f7773` stream lint cleanup moves no milestone marker.** Replacing a redundant closure
    with the identical `Write::write_all` method item changes neither the v3 stream format nor its
    production-dark reachability and adds no milestone evidence; M3 stays `[partial]`.

    **Authoritative PTY lifecycle and input recording in this commit moves no milestone marker.**
    The host now orders raw output, resize, content-independent input evidence, and typed terminal
    End records in the versioned stream; propagates the bounded root wait's timeout truth; and
    visibly fails only the exact negotiated pane-v1 connection when opaque input cannot be
    evidenced or delivered. The causal evidence is
    `opaque_input_is_admitted_through_every_valid_pane_slot_phase`,
    `pane_input_failure_cancels_only_the_exact_pending_subscription`,
    `pane_input_failure_visibly_cancels_an_in_flight_transition`,
    `opaque_input_reaches_the_slave_exactly_without_a_legacy_i_record`,
    `output_quota_exhaustion_does_not_disable_the_next_opaque_key`,
    `input_evidence_followed_by_master_delivery_failure_seals_incomplete_end`,
    `opaque_evidence_failure_refuses_master_delivery_and_completion`,
    `authoritative_stream_orders_output_resize_input_and_typed_end_densely`,
    `opaque_input_evidence_refusal_closes_only_that_pane_client_before_end`,
    `admitted_wire_opaque_input_failure_wins_over_terminal_end`,
    `unresolved_pending_wire_input_is_visibly_failed_before_terminal_end`,
    `legacy_attach_cannot_forge_opaque_pane_input`,
    `legacy_input_keeps_its_compatibility_delivery_on_durable_evidence_failure`, and
    `bounded_terminal_timeout_is_recorded_in_the_recovered_session_end`. Production serving still
    installs the disabled native bootstrap service and the shipped CLI still refuses the native
    facade relay, so native command execution remains dark and M3 remains `[partial]` pending C1's
    recorded manual session.

    **Sharing the pane-v1 client decoder in this commit moves no milestone marker.** The extraction
    preserves rendered attach's byte-exact Output, Resize, dense sequence, replay-cut, and ordered
    End behavior while making those already-committed client invariants reusable. Direct decoder
    tests supplement the existing rendered-terminal evidence, but no native relay is activated or
    shipped by this refactor. M3 therefore stays `[partial]`, with C1's recorded manual session
    still outstanding.

    **The causal pane-drain test correction in this commit moves no milestone marker.**
    `closing_releases_the_registry_before_shutdown_drains_admitted_control` now waits for a
    test-only signal that shutdown observed the exact unresolved input-delivery barrier instead of
    treating a timeout equal to the production grace period as proof of ordering. It then proves
    `Closing` released the global pane registry, injects the master-write failure, and releases the
    delivery. Production behavior is unchanged, so M3 remains `[partial]` pending C1's recorded
    manual session.

    **Native relay foundation and terminal restoration in this commit move no milestone marker.**
    The compiled, production-dark raw pane-v1 relay preserves opaque input and output, enforces
    attach/Ready, replay-cut, dense-sequence, and End boundaries, and forwards resize under
    exclusively owned, restored SIGWINCH handling. Its retained terminal guard arms before the first
    termios or file-status mutation, retries interrupted syscalls, independently restores stdin
    flags, stdout flags, and termios, preserves queued input, retains retry ownership after partial
    cleanup, and appends cleanup failure without replacing the primary failure. Tests cover raw
    protocol boundaries, SIGWINCH restoration, contention, and reset across relay exits, terminal
    entry rollback and partial restoration, Drop retry, and production-path open-failure composition.
    Compiling that relay also retires stale dead-code expectations on the claim decomposition methods
    it now consumes. No non-SIGWINCH signal lifecycle is implemented: job control,
    SIGINT/SIGTERM/SIGHUP handling, panic and passive cleanup, nested-PTY end-to-end evidence,
    production descriptors, service enablement, and CLI activation remain outstanding. M3 stays
    `[partial]` pending C1's recorded manual session.

    **Native relay suspend state and signal ownership in this commit move no milestone marker.**
    `NativeRelayTerminal` now explicitly models `Raw`, `Restored`, and `RestorePending`, with
    reviewed suspend restoration and raw-mode re-entry primitives that remain production-dark until
    the relay event pump consumes them. One process-global guard now atomically captures,
    installs, rolls back, and exactly restores WINCH/INT/TERM/HUP/TSTP actions on the supported
    macOS/Linux aarch64/x86_64 targets; its handlers publish only lock-free atomic state, overlapping
    owners are refused, and ownership is reacquirable after cleanup. External signal consumption and
    same-signal redelivery, the TSTP/CONT lifecycle, passive terminal cleanup and panic restoration,
    real nested-PTY end-to-end evidence, production descriptors, service enablement, and CLI
    activation remain outstanding. These compiled foundations satisfy no activation criterion, so
    no marker advances and M3 remains `[partial]`.

    **Synchronous native-relay signal design recorded on 2026-08-13 moves no milestone marker.**
    The production-dark process-global guard foundation already committed in `a4c065b` remains, but
    v5 narrows and replaces its activation model: attach precedes signal and raw activation; only
    default, unblocked termination and job-control signals are owned; ignored or blocked signals are
    untouched; `SIGTSTP` is never synthesized; terminal output is bounded and nonblocking; and exact
    terminal, action, and mask cleanup is required. A later termination/redelivery/lifecycle expansion
    was experimental and remains uncommitted: its multithreaded publication/teardown race and PTY
    flake evidence caused its rejection. This is design rationale only, with no production activation
    or runtime-test claim; M3 remains `[partial]`. macOS/Linux runtime, INT/HUP/TSTP/CONT, and
    socket/lease/node-survival end-to-end evidence remain outstanding.

    **Authenticated native command lifecycle in this commit moves no milestone marker.** Native
    selection can now launch a supervisor-owned PTY process, reserve its exact pending writer, and
    pre-create both lifecycle and claimed-connection workers behind abortable gates before the
    decisive claim acknowledgement. Timeout reset, bounded SIGPIPE-safe acknowledgement, rollback,
    child reaping, and ticket/lease retry are covered by
    `lifecycle_spawn_failure_preserves_launch_for_retry_without_lifecycle_effects`,
    `real_claim_preparation_reserves_writer_and_abort_reaps_without_starting_lifecycle`,
    `lifecycle_thread_spawn_failure_precedes_claim_ack_and_keeps_retry_available`,
    `client_timeout_reset_failure_sends_no_native_claim`,
    `claim_ack_deadline_is_bounded_by_the_absolute_launch_deadline`,
    `blocked_claim_ack_flush_does_not_start_the_prepared_relay`,
    `interrupted_claim_ack_send_retries_before_relay_start`,
    `claim_ack_write_failure_aborts_prepared_relay_without_starting_or_connecting`,
    `claim_timeout_reset_failure_precedes_ack_and_relay_start`,
    `claimed_connection_preflight_failure_precedes_ack_and_relay_start`,
    `launch_deadline_expiring_during_real_preflight_refuses_without_starting`, and
    `panicking_request_cancel_still_revokes_real_pending_authority`, while
    `ordinary_connection_is_announced_exactly_once` and
    `prepared_connection_abort_cannot_lose_its_writer_wakeup` protect the shared connection path.
    Production serving still installs the disabled native bootstrap service, the shipped CLI still
    refuses the native facade relay, and terminal/signal relay restoration is intentionally outside
    this commit. M3 therefore remains `[partial]` pending C1's recorded manual session.

    **PTY stream repair diagnostic hardening in this commit moves no milestone marker.** A single
    default-parallel run failed before preserving the assertion operands; the exact test and 1,024
    concurrent stress repetitions then passed. The test now reports the actual stream error and
    whether its injected fault was reached, so a recurrence distinguishes recovery logic from a
    pre-injection open/lock failure. The root cause remains unconfirmed, production behavior is
    unchanged, and M3 remains `[partial]` pending C1's recorded manual session.

    **Native relay signal ownership through real exit paths in this commit moves no milestone
    marker.** The relay now acquires exclusive WINCH/INT/TERM/HUP/TSTP ownership before entering
    raw mode, refuses while any owned signal is blocked on its thread, records the first
    INT/TERM/HUP, stops pumping on it, and finishes every exit (End, detach, protocol or I/O
    failure, external signal) through one stage that writes passive terminal cleanup bytes,
    restores termios and descriptor flags, restores the prior actions termination-first, and
    synchronously redelivers the recorded signal to the restored disposition while ownership is
    still held. Failed restoration poisons ownership and leaves Drop armed to retry. Covered by
    `relay_signal_guard_refuses_each_blocked_owned_signal_without_side_effects`,
    `finish_redelivers_synchronously_while_signal_owner_is_held`,
    `signal_published_during_action_restoration_is_not_cleared`,
    `every_failed_signal_restoration_remains_armed_for_drop_retry`,
    `native_relay_restores_the_prior_sigwinch_handler_after_every_exit`,
    `native_relay_sigterm_restores_terminal_and_redelivers_to_itself` (nested-PTY child), and the
    raw-terminal probes on the production `relay_claimed`/`finish_claimed_relay` path, which now
    live in `native_relay` so `native_tty` no longer imports the module that depends on it. The
    production native facade stays dark: `run` is still unreachable from the shipped CLI, TSTP is
    recorded but suspend/resume is not implemented, and no end-to-end socket/lease evidence is
    claimed. M3 remains `[partial]` pending C1's recorded manual session. *(2026-09-05: all
    three sentences superseded — `run` is reached from the shipped CLI, Ctrl-Z suspend/resume is
    implemented, and `native_facade_e2e.rs` is the end-to-end evidence.)*

    **The client environment on the native bootstrap wire in this commit moves no milestone
    marker.** `DirectNativeRequestContext` now carries the caller's process environment as a
    trailing, length-framed section that is debited from the same aggregate context budget,
    bounded by entry count, stripped of `MARION_*` on the client, refused by the decoder if a
    reserved name still arrives, and excluded from the BLAKE3 context hash (the fixed hash vector
    is unchanged). Covered by `authorized_launch_sees_the_client_environment_not_the_supervisors`,
    `environment_debits_the_same_aggregate_budget_and_is_bounded_by_count`,
    `reserved_marion_environment_on_the_wire_is_a_protocol_violation`, the extended
    `context_hash_has_a_fixed_vector_and_every_semantic_boundary_is_load_bearing`, and the extended
    `ordinary_json_rpc_rejects_copied_native_token_before_capability_lookup`, which now also sends
    the exact env-bearing native frame over the ordinary socket. Production serving still installs
    the disabled native bootstrap service, so nothing consumes the environment yet; M3 remains
    `[partial]`.

    **The production native command factory in this commit moves no milestone marker.**
    `ProductionNativeCommandFactory` resolves the descriptor's declared executable on the client's
    `PATH` (never the supervisor's), builds the node's marion MCP bridge declaration, hands the
    opaque adapter only `NativeNodeContext`, and lets `assemble_native` place
    `program + prefix + opaque_tail` with the client environment plus overlay; documents are
    refused fail-closed until the dirfd-relative artifact executor exists. The adapter lookup is a
    constructor argument (`NativeAdapterLookup`) so the registry-driven adapters can be supplied
    later without a vendor branch here. Covered by
    `production_factory_assembles_program_prefix_then_opaque_tail_byte_exact` (tail with `--`, an
    empty argument, invalid UTF-8, and a spelling that collides with the prefix) and
    `production_factory_refuses_without_a_client_resolvable_executable_or_an_adapter`. The factory
    is still production-dark: no serving path constructs it, and the production descriptor slice is
    empty. M3 remains `[partial]`.

    **The process-table terminal verifier in this commit moves no milestone marker, and it
    corrects a claim the earlier records implied.** `verify_bootstrap_tty` and `revalidate_peer`
    proved "same controlling terminal, foreground process group" with `tcgetsid`/`tcgetpgrp` on the
    received descriptors. Both ioctls answer `ENOTTY` on Linux and macOS unless the terminal is the
    **caller's** controlling terminal, so the production verifier could never have authorized a
    launch from a detached supervisor; the "real TTY" unit fixtures passed only because they ran
    the service inside the client's own session. The verifier now reads the peer's controlling
    terminal device and foreground process group from the kernel process table (macOS
    `sysctl(KERN_PROC_PID)` → `kinfo_proc.kp_eproc.{e_tdev, e_tpgid}` at measured offsets 572/576;
    Linux `/proc/<pid>/stat` `tty_nr`/`tpgid`) and compares them with `fstat` of the descriptors
    and `getpgid(peer)`. Covered by
    `production_verifier_proves_the_peer_terminal_from_another_session`, which verifies a
    foreground child in a fresh session from the test's own session and refuses a session child
    that merely holds the descriptors. The Linux arm is written to the documented `/proc` layout
    and is unmeasured here. M3 remains `[partial]`.

    **The enabled native bootstrap service in the detached supervisor in this commit moves no
    milestone marker.** `detach.rs` stage 3 now starts the server through
    `Server::start_with_native_launch`, which composes the production command factory,
    `NativeLaunchHandler`, and the pending-launch authority onto the private native socket;
    `start_with_idle_grace` keeps the disabled service for library callers. The shipped CLI
    dispatch now carries the operator's environment in the bootstrap request. Covered end to end by
    `a_detached_supervisor_authorizes_a_native_launch_for_a_registered_facade`: a supervisor
    composed through the same public constructor, one test-registered `claude` facade with a
    fixture adapter, the shipped `dispatch_native_facade_or_legacy` re-executed on its own
    controlling PTY from another session, the real private socket, and a `claude` shim found only
    on the client's `PATH` that recorded `--marion-injected`, `--`, an empty argument, and invalid
    UTF-8 byte for byte, saw the client's `PATH` and the adapter overlay, saw no `MARION_*`
    identity, and exited 37. What stays dark, deliberately: the production descriptor slice is
    empty and the production adapter table answers `None` for every harness, so a real `marion
    claude` still takes the legacy path and no vendor lane is reachable; the client does not yet
    claim or relay (the pending launch is cancelled when the bootstrap connection closes); and the
    vendor's cwd is the canonical project root rather than the operator's subdirectory. M3 remains
    `[partial]` pending the registry-driven adapters, facade advertisement, the CLI relay, and
    C1's recorded manual session. *(2026-09-05: the first four of those are landed and named in
    the paragraphs below; the vendor's cwd being the project root rather than the operator's
    subdirectory is the one clause here still true.)*

    **Mask-owned `SIGTSTP` in this commit moves no milestone marker.** Per the v5 synchronous-signal
    design, the relay no longer installs a `SIGTSTP` handler. A default, unblocked `SIGTSTP` is
    owned by blocking it on the relay thread and observing it pending without consuming it; an
    ignored or already-blocked `SIGTSTP` is excluded and untouched; a custom `SIGTSTP` action
    refuses acquisition before any mutation. Exact restoration unblocks it again. Covered by
    `relay_signal_guard_blocks_a_default_sigtstp_instead_of_handling_it`,
    `relay_signal_guard_leaves_an_ignored_sigtstp_untouched`,
    `relay_signal_guard_excludes_an_already_blocked_sigtstp`, and
    `relay_signal_guard_refuses_a_custom_sigtstp_disposition_without_side_effects`. The relay pump
    does not yet act on a pending stop, so Ctrl-Z behavior in the dark facade is unchanged in
    effect; M3 remains `[partial]`.

    **Pending-stop observation in the relay pump moves no milestone marker.** Each 50 ms tick now
    queries `sigpending` for the owned default `SIGTSTP` and returns `RelayStop::Suspend` without
    consuming it, ahead of the detach check. Until the suspend transition lands, a Ctrl-Z on the
    dark facade would finish the relay like a clean detach with the terminal restored. Covered by
    `pump_reports_a_pending_owned_stop_as_suspend_without_consuming_it`; M3 remains `[partial]`.

    **The relay's Ctrl-Z suspend/resume transition in this commit moves no milestone marker.** When
    the pump reports a pending stop, the relay blocks its termination handlers, joins the keyboard
    worker, writes the passive cleanup bytes, restores termios and descriptor flags, lets a
    termination that beat the cutoff win, and only then reveals the still-pending `SIGTSTP` to its
    untouched default action. After `SIGCONT` it reblocks the stop, re-exposes the termination
    handlers, re-enters raw mode, restarts the keyboard worker, and forces geometry reconciliation;
    a failed raw re-entry rolls back to the cooked baseline and ends the relay with an error. The
    supervisor's node child is never signalled, so it keeps running behind the relay. Covered in
    process by `suspend_restores_the_terminal_then_reenters_raw_and_forces_a_resize`,
    `suspend_writes_passive_cleanup_bytes_before_the_stop`, and
    `a_failed_raw_reentry_after_continue_rolls_back_to_the_cooked_baseline`, which drive the
    transition through its SIGCONT-cancelled-before-reveal path. A real kernel stop cannot be
    observed from the existing PTY child probe because `spawn_pty` puts the child in an orphaned
    process group, where the kernel discards `SIGTSTP`; the reveal/resume syscall sequence itself
    was verified to stop and continue a non-orphaned process out of band. The production native
    facade stays dark, so M3 remains `[partial]` pending C1's recorded manual session.

    **Pre-cutoff termination dominance across the stop moves no milestone marker.** A termination
    that arrives while the relay is stopped stays blocked and pending; when the resume unblocks the
    termination handlers it is recorded, and the relay now finishes through the termination path
    with the terminal still restored instead of re-entering raw mode. Covered by
    `a_termination_during_the_stop_wins_after_continue_before_raw_reentry`; M3 remains `[partial]`.

    **The tree screen's 2026-09-05 UX pass moves no milestone marker.** A screenshot review of
    `marion tree` at 80x24, 120x40 and 160x50 found the screen technically complete and
    operationally mute, and each fix below is one microcommit with its own render-to-buffer test;
    M3 stays `[partial]` and M5 clause 3's greying is unchanged in substance.
    - Rows lead with the state and label an unnamed node `agent_type short-id` instead of its
      UUID, whose state the 36-column tree had clipped on every real node; the column is 44 now.
      `a_row_leads_with_its_state_so_the_column_clip_cannot_hide_it`,
      `an_unnamed_node_is_labelled_by_type_and_short_id_and_a_named_one_by_its_name`, and the L4.5
      snapshots re-accepted over the new rows.
    - The content pane is a detail pane instead of a two-line hint: name, agent type, harness and
      version (or `version unknown`), state with reap state, pane/headless surface, depth, parent,
      timeout and the whole id, with the key hints as its last row saying `Enter` opens pane nodes
      only and `^] d` detaches. `the_detail_pane_names_every_fact_the_row_cannot_carry`.
    - `Enter` on a headless node is refused from `NodeSummary::pane` before the terminal changes
      hands, and the reason is drawn bold in the detail pane until the cursor moves; an attach's
      own refusal lands there too instead of on a stderr the next frame erased.
      `enter_on_a_headless_node_is_refused_in_the_pane_and_a_pane_node_opens`.
    - The draw loop re-measures the window every pass and resizes the `ScreenBackend`, so a tree
      dragged to another size repaints at that size instead of at the one it was entered with.
      `a_resize_is_applied_on_the_next_pass_and_a_pipe_keeps_the_last_geometry`.
    - `Screen::enter` clears and homes the alternate buffer right after switching to it, so a pane
      opened from the tree no longer shows tree fragments wherever its first frame was blank.
      `entering_switches_the_buffer_before_it_enables_a_mouse` pins the byte order.
    - The capability strip is labelled `caps:`, draws an absent capability dim rather than struck
      through (unmeasured, not cannot), an offered one bold, and appends `harness version unknown`
      when the key's version was never read. Doctor remains the only source of the bits;
      `a_greyed_action_differs_from_an_offered_one_only_in_style`,
      `the_strips_note_follows_the_actions`, `an_unread_version_is_named_on_the_strip`, and the
      L4.5 snapshots re-accepted over the new styles.
    - A status row along the top — `marion · <project> · N nodes, M running`, where running is the
      complement of §7.6's terminal set — so an operator with several terminals open can tell which
      project and how big a forest each one is. `the_status_row_names_the_project_and_counts_the_forest`,
      `the_status_rows_running_count_is_the_non_terminal_nodes`, and the L4.5 snapshots re-accepted
      with the row in the payload.
    - Entry prose trimmed: the no-supervisor refusal is one sentence plus the command, a paned
      root's `marion run` prints one line naming `marion attach`, `^] d` and `marion tree`, and
      the §7.3.2 detach disclosure is one line for the fleet — running nodes, `marion tree` /
      `marion attach`, the stop call — plus a second line only when a node is exposed at a
      permission gate.
      `the_no_supervisor_refusal_is_one_sentence_and_the_command_to_run`,
      `a_detach_with_nothing_at_a_gate_reports_in_one_line`.

    **Row-derived native injection in this commit moves no milestone marker.** Every
    `HarnessSpec` row now states its live MCP declaration channel in full (`LiveDeclaration`:
    claude `--mcp-config <file>`, codex `-c mcp_servers.marion.*`, gemini
    `GEMINI_CLI_SYSTEM_SETTINGS_PATH` document, opencode `OPENCODE_CONFIG_CONTENT`, copilot
    `--additional-mcp-config @<file>`; ACP none), and one `SpecNativeAdapter` renders it — there
    are no per-harness native impls, and `marion_harness::native_adapter(Harness)` is a table of
    rows. `NativeNodeContext` now carries the typed `BridgeEnv` so the factory and every managed
    document derive the bridge's variables from one place. Covered by
    `every_native_row_injects_only_marions_mcp_server_and_no_managed_flags`, which holds each
    harness's native injection byte-for-byte to what the managed live launch of the same row
    writes and to nothing else. Still dark: the production descriptor slice is empty and documents
    remain fail-closed; M3 remains `[partial]`.

    **The detached supervisor's adapter table is the harness registry's, moving no milestone
    marker.** `detach.rs` stage 3 hands `Server::start_with_native_launch` the row-derived
    `marion_harness::native_adapter` in place of the always-`None` placeholder, so the production
    composition and the integration bed differ only in which table they are handed. Nothing is
    reachable yet: the production descriptor slice is still empty; M3 remains `[partial]`.

    **Native declaration documents in this commit move no milestone marker.** The production
    factory now writes a row's declaration document instead of refusing every document-carrying
    harness: only a direct child of the node's own directory, opened through a descriptor on that
    directory with `O_NOFOLLOW|O_CREAT|O_EXCL` at `0600`, fsynced, and refused by name — writing
    nothing — for any other path or an existing name. Covered by
    `production_factory_materializes_a_row_document_under_the_node_dir_only`. The claude, gemini
    and copilot rows can therefore be assembled; the descriptor slice is still empty, so M3 remains
    `[partial]`.

    **The production registry advertises every terminal harness in this commit, moving no
    milestone marker.** `PRODUCTION_NATIVE_FACADES` now carries one descriptor per `Harness` minus
    `Acp` — `claude`, `codex`, `gemini`, `opencode`, `copilot`, each command the executable it
    stands in front of, each adapter id the harness's own row — with `claude` and `codex` enabled
    (their TUI rows were measured with the declaration flag) and the other three disabled by a
    named reason (gemini's system-settings merge granularity; opencode's and copilot's channel
    measured only headless). *(2026-09-05, later: eight descriptors, since `goose`, `cline` and
    `qwen` joined `Harness`; `opencode` and `copilot` enabled on measurement the same day, so four
    lanes are enabled and four — `gemini`, `goose`, `cline`, `qwen` — are disabled by name.)* Covered by `production_facades_cover_every_terminal_harness_exactly_
    once` (cardinality from the enum) and, at the shipped binary, by `registered_selectors_refuse_
    off_a_terminal_while_reserved_and_unknown_names_keep_legacy_usage`: every registered selector
    off a terminal now takes the facade's TTY refusal instead of legacy usage, while `run`,
    `attach`, `tree`, `mcp`, the reserved words and unknown names stay legacy byte for byte. The
    shipped CLI still answers an authorized handoff with "relay is not enabled", so M3 remains
    `[partial]`.

    **The shipped CLI relays a native facade in this commit, moving no milestone marker.**
    `main`'s authorized continuation is now `facade_cli::relay_native_facade`, which claims the
    launch and runs `native_relay::run` to its end, printing a refusal only after the terminal is
    restored. Running it exposed that a native node had no journal identity — `node/attach`
    resolves a node from the registry before it looks for a pane — so the native launch handler
    now writes §6.1 step 7's `SpawnIntent` (a root: `parent_id: None`, `depth: 0`, no task)
    before any side effect and folds it into the live tree before answering, the launcher writes
    `Spawned` with the pid and start identity at `spawn()`, the lifecycle worker writes `Exited`
    with the observed status, and every rollback or refusal after the intent writes
    `SpawnAborted` (`native_exec::NativeNodeRecorder`). Covered by the extended
    `a_detached_supervisor_authorizes_a_native_launch_for_a_registered_facade`, whose client now
    relays through the shipped continuation: the vendor shim's own line reaches the operator's PTY,
    the client exits cleanly when the vendor exits 37, and the replayed journal holds exactly one
    node with the intent, a pid, and `Exited{code: 37}`. Still unproven from the shipped binary
    itself; M3 remains `[partial]`.

    **One process-table read in this commit moves no milestone marker.** The peer-terminal
    verifier's second `sysctl(KERN_PROC_PID)` is gone: `procid::kinfo_proc` is the crate's one
    such call, `procid::read` and the new `procid::controlling_terminal` (macOS `e_tdev`@572 /
    `e_tpgid`@576, Linux `/proc/<pid>/stat` fields 7/8 with every missing field an error) both
    read through it, and `native_tty` keeps a single `verify_peer_terminal` behind
    `verify_bootstrap_tty` and `revalidate_peer`. Covered by the causal
    `production_verifier_refuses_a_different_terminal_and_a_background_peer`: a job-control
    leader on PTY A is `NotControllingTerminal` through descriptors on PTY B, and its background
    job is `BackgroundProcessGroup` through descriptors on A; disabling either comparison fails
    the test. M3 remains `[partial]`.

    **Visible input-failure semantics in this commit move no milestone marker.** The supervisor
    refuses opaque pane input by closing the negotiated connection (`Departure::PaneInputFailed`),
    so the relay now reads a pre-End EOF in causal order: the keyboard worker's own failure first,
    a clean detach second, then — if a keystroke had been forwarded — a named refusal that says the
    supervisor refused the relay's keyboard input, and only otherwise the plain "closed before
    End". A keyboard worker that stops for any reason shuts the socket's read side so the pump
    wakes at once instead of at its next 50 ms poll, and the pump reports the worker's message
    rather than the EOF it caused. The message survives `finish_claimed_relay` as the primary error
    and `relay_native_facade` prints it after termios restoration. Covered by
    `a_supervisor_input_refusal_ends_the_relay_visibly_after_terminal_restoration` (via
    `open_for_test`) and `a_keyboard_failure_wakes_the_pump_immediately_with_its_own_message`
    (elapsed under the poll interval). M3 remains `[partial]`.

    **The shipped binary in front of a real harness, as a smoke proof, moves no milestone
    marker.** `tests/native_facade_smoke.rs` composes a supervisor exactly as `detach.rs` stage 3
    does — `PRODUCTION_NATIVE_FACADES`, `marion_harness::native_adapter`, the production factory —
    and runs the **shipped `marion codex --help`** on its own controlling PTY from another session.
    Observed on darwin with codex 0.147.0: the client exits 0, `Usage: codex` from the harness's
    own help text reaches the operator's PTY through the relay, the PTY's termios after the run
    equals its baseline, and the journal replays one native `codex` root with a pid and
    `Exited{code: 0}`. It skips loudly when `codex` is not on `PATH`. This is one lane, one flag,
    and no keystroke: the M3 matrix (a TUI cell on screen, input reaching the child, resize,
    detach leaving the node in the tree, single-use re-attach refusal, signal restoration across
    every enabled harness) is `plan-native-activation.md` step 9 and remains open, so M3 remains
    `[partial]`.

    **The relay's terminal writer waits out a full output queue (2026-09-05); no marker moves.**
    Found by the first run of the M3 E2E matrix, not by any unit test: the shipped `marion claude`
    and `marion codex` clients left on their own within seconds of their first screen, exit 1, with
    the operator's screen holding a `?2026h`-opened frame that never closed — 1233 of the node's
    1506 bytes. Mechanism: `native_relay::OwnedFdWriter::write` was a bare `write(2)` on the
    terminal descriptor the relay makes nonblocking at activation, and `Write::write_all` returns
    `WouldBlock` as an error, so a TUI's one-burst frame larger than the pty's output queue ended
    the relay mid-frame whenever the operator's terminal read a little slower than the harness
    painted. A refused write now blocks on the descriptor's writability — `poll(2)` for `POLLOUT`
    with the remaining deadline, declared by hand as `marion_tui::guard` declares it — for up to
    10 s, and only then reports a named stall (nothing is reading the operator's terminal).
    RED/GREEN: `output_writes_wait_out_a_full_nonblocking_terminal_queue` writes 1 MiB through the
    writer to a nonblocking socket pair drained 4 KiB per 2 ms; it failed with `WouldBlock` (os
    error 35) on the old writer and passes with every byte counted on the new one. M3 remains
    `[partial]`.

    **Open, recorded rather than fixed here:**
    `native_relay_sigterm_restores_terminal_and_redelivers_to_itself` is flaky under the parallel
    `cargo test -p marion-supervisor --lib native_relay` filter on the tree *before* this change
    too (1 of 2 runs at the same HEAD; the bounded inner probe exits 101 after its
    `sigterm-observed-wait-complete` phase) and passes 6/6 run alone. The suspect is the two PTY
    signal probes sharing the process-wide relay signal owner; it needs serial isolation, not a
    wider bound. Fixed at `dcd907d`; the suspect was wrong: a session leader's `proc_exit` drains
    its controlling PTY before it becomes reapable, so an unread master left the probe a zombie for
    the whole bound. The fixture now drains the master while waiting (next paragraph).

    **The SIGTERM PTY flake was the fixture, not the signal owner (2026-09-05); no marker
    moves.** Reproduced 2/10 then 5/12 under the parallel filter, every failure `the SIGTERM relay
    probe did not exit naturally within 500ms` with `ps` showing the probe already `Z`, and a
    single `read` of the master reaping it 12 µs later. Mechanism: the probe is the session leader
    of its controlling PTY, and `proc_exit` drains a session leader's controlling terminal before
    the process becomes reapable. The probe writes the passive cleanup bytes (`?2004l` plus the
    TUI leave sequence) *after* `NATIVE_SIGTERM_OBSERVED`, and the inner fixture stopped reading
    the master once it saw that marker; whether the cleanup bytes were consumed by the same read
    or left in the slave output queue was a 1 ms scheduling race, which the parallel run lost
    about half the time. No process-wide state in the parent test process was involved: the probe
    and the inner runner are already separate processes, and `RELAY_SIGNAL_OWNER` was never
    contended. Fix in `native_relay.rs`: `drain_pty` reads the master on every `try_wait` poll in
    both PTY fixtures (a terminal never stops reading), and the SIGTERM fixture now also asserts the
    passive cleanup bytes crossed the PTY. `cargo test -p marion-supervisor --lib native_relay`:
    20/20 green after the fix.

    **The M3 native facade matrix runs, harness-generic, from the shipped binary (2026-09-05);
    M3 stays `[partial]`.** `tests/native_facade_e2e.rs::every_enabled_native_lane_runs_its_real_tui_through_the_shipped_facade`
    iterates `production_native_facades().enabled_native_commands()` — `claude` and `codex` when
    this was written, four lanes since the `opencode`/`copilot` paragraph below — and for each runs the **shipped** `marion <harness>` with no tail (the TUI's first screen,
    which asks nothing of a provider) as a foreground session leader on a PTY marion records, in
    front of a supervisor composed as `detach.rs` stage 3 composes it. Observed on darwin 25.5.0
    with claude 2.1.261 and codex 0.147.0, 3/3 loops after the writer fix above, both lanes:
    (1) a word the harness painted (`Accessing` from claude's trust dialog, `INTERACTIVE` from
    codex's first screen) is read off the operator's grid through `marion_term` *and* found in the
    node's own recorded output, so the cell came through the relay; (2) a keystroke (an arrow, then
    a letter, each re-sent only after a 2 s settle) grows the node's recorded output and leaves
    **no `i` record** in its `pty.cast` — native input goes through the opaque path, not the legacy
    pane write; (3) `TIOCSWINSZ` on the operator's master yields an `r` record with the operator's
    geometry among at least two; (4) `^]d` exits the client 0 with termios equal to the pre-spawn
    baseline, and the journal still holds exactly one node with no exit; (5) a confirmed
    `session/quit KillTree` then ends it and the journal's `Exited` carries `signal 9` — positive
    evidence the node was alive after its client left. Two of step 9's clauses are stated in the
    file rather than asserted, because the fixture cannot observe them: single-use re-attach (the
    capability dies with the client; proved at `native_bootstrap.rs`) and default signal actions
    in a probe child (no child of the client exists; the SIGTERM test below asserts the observable
    consequence). It skips loudly per lane when the harness is not on `PATH`.

    **`SIGTERM` at the shipped client, per the synchronous-signals design (2026-09-05); no marker
    moves.** `native_facade_sigterm_restores_the_operator_terminal_and_exits_by_signal`, same
    fixture, every enabled lane: `kill(SIGTERM)` to `marion <harness>` at its first screen; the
    client's wait status is *terminated by signal 15* (only possible once the relay restored
    `SIG_DFL` and unblocked the signal), the operator's termios equals the pre-spawn baseline, the
    journal still holds one unexited node (marion neither kills nor signals the node — the design's
    "survives for ordinary reconnect"), and a confirmed `KillTree` then ends it with `Exited{signal
    9}`. Green for claude and codex.

    **`SIGTSTP`/`SIGCONT` through this fixture: measured not observable, and not asserted
    (2026-09-05).** A probe on the same fixture sent `kill(SIGTSTP)` to the `marion codex` client
    at its first screen and polled `waitpid(WUNTRACED)` for 5 s: no stop was ever reported, the
    client kept running in raw mode. That is the caveat the TSTP commits named, now measured:
    `spawn_pty` makes the client a session leader whose parent lives in another session, so its
    process group is *orphaned* and the kernel discards stop signals for it (Darwin `issignal`,
    Linux `do_signal_stop`) whether Marion reveals the pending stop or not. A fixture that can see
    the stop needs a parent in the client's own session and a different process group (a job-control
    shell as session leader running the client as a foreground job), which is a different fixture
    and is not built here. Stop/resume stays covered where it is observable: the isolated
    `suspend_probe` tests in `native_relay.rs`. No E2E claim is made about TSTP. That different
    fixture was built the same day at `acb1cc5`:
    `native_relay::tests::native_relay_stop_and_continue_are_observed_by_a_job_control_leader`
    (next paragraph).

    **A real kernel stop and continue through the relay is observed (2026-09-05); no marker
    moves.** `native_relay::tests::native_relay_stop_and_continue_are_observed_by_a_job_control_leader`
    is the job-control fixture the paragraph above asked for, at the unit boundary: the inner
    runner owns a PTY master and `spawn_pty`s a *leader* (session leader, the slave as its
    controlling terminal); the leader `fork`s the probe into a new process group of the same
    session that claims the foreground itself (`SIGTTOU` blocked around one `tcsetpgrp`), so the
    group is not orphaned. The probe acquires signal ownership, then enters raw mode, which the
    leader reads as the ready oracle (`tcgetattr` on its own stdin). The leader sends `SIGTSTP` to
    the group, observes `WIFSTOPPED` with `SIGTSTP` through bounded `waitpid(WUNTRACED|WNOHANG)`,
    asserts the terminal is at its cooked baseline while the job is stopped, sends `SIGCONT`,
    polls the terminal back into raw mode, sends `SIGTERM`, observes the exit by signal 15, and
    asserts the baseline again; the inner asserts the byte order on the master — ready marker,
    passive cleanup bytes, stopped marker, resumed markers — and the PTY's final termios. Every
    child is owned by its direct parent with bounded polls and `SIGKILL` plus `waitpid` at the
    ceiling; no `std::process::Child::try_wait` is used for the job because it never passes
    `WUNTRACED`. 10/10 alone and 20/20 inside the parallel `native_relay` filter (50 tests).
    *What was tried first and why it is not the fixture:* re-exec'ing the test binary as the probe
    stopped the job with the terminal still raw. libtest's harness main thread does not block
    `SIGTSTP`, and a process-directed stop is delivered to any thread that does not block it, so
    the kernel default stopped the process before the relay could reveal anything. The relay's
    ownership model (block on the relay thread, later threads inherit) holds for the shipped
    binary because `main` runs the relay on its only thread; it would not hold for a client that
    spawned any thread before `RelaySignalGuard::acquire`. The fixture therefore forks a
    single-threaded probe and joins the pre-ownership server thread before entering raw mode.

    **The `opencode` and `copilot` native lanes are enabled on measurement (2026-09-05); no
    marker moves.** Their rows had said the declaration channel was measured only on the headless
    surface. Measured now on the interactive shape, through the shipped facade and the matrix
    fixture on darwin 25.5.0: **opencode 1.17.3** (`OPENCODE_CONFIG_CONTENT`) — its `/mcp` dialog
    lists `marion connected ✓ Enabled` beside the operator's own `pencil` and `semble` servers, so
    the inline document merges with the user's config rather than replacing it, and the status bar
    counts marion in `⊙ 2 MCP`; **copilot 1.0.83** (`--additional-mcp-config @<path>`) — its `/mcp`
    view lists `marion · User · mcp:marion · 646 tokens` beside the built-in `github-mcp-server`,
    `2/2 enabled`, and a token count is a `tools/list` the bridge answered. With the two flags
    flipped, `every_enabled_native_lane_runs_its_real_tui_through_the_shipped_facade` runs four
    lanes and passes all clauses for each (`opencode` first word `anything`, `copilot` first word
    `Sessions`), and the registry pin
    `production_facades_cover_every_terminal_harness_exactly_once` now names the four. One
    side-effect to know about: opencode's first screen was an *update prompt*, and the probe's
    first attempt pressed Enter on it, which started opencode's self-update on this machine; the
    committed matrix never presses Enter.

    What remains for C1 is unchanged and cannot be automated: the recorded 10-minute manual
    session, now to be run through `marion claude` rather than `marion attach`. Still dark: the
    `gemini` lane (system-settings merge unmeasured) and TSTP/CONT through this fixture.

    What the tree does correct is a count this repo had written down twice and got wrong twice.
    `marion-tui/src/lib.rs` and `view.rs` both justified deferring the tree with *"M3's acceptance
    criteria (§9) mention a pane three times and a tree zero times"*. Re-counted against §9's M3
    block: **pane 5×, tree 1×** — the one being the title. The deferral was still defensible on the
    bullets, and it is not being retro-fitted into a mistake; the arithmetic under it simply never
    held, and both comments now say so. What actually forced the work was **M5 clause 3**, which is
    a different milestone and is ruled on below.

### M3 C1's recorded manual session — the runbook

**This is the one thing left between M3 and `[done]`, and it needs a person.** Everything else in
C1 is asserted by a test; §9 ends the criterion with *"over a recorded 10-minute manual session"*,
and no automation can perform it. Written down so whoever sits down does not have to re-derive it.

**Before starting.** `git config core.hooksPath .githooks` if this is a fresh clone. Run
`claude --version` and check it against `PINNED_HARNESSES` **first**: if the installed binary is
not admitted, either put an admitted one first on `PATH` (they stay on disk under
`~/.local/share/claude/versions/`, so a one-line shim is enough) or run the admission ritual
`9b0a06d` performs before recording anything. An unpinned binary makes the recording
unattributable, and §5.3's Claude readings have already gone stale twice under sessions that
skipped this. Have `asciinema` on `PATH`.

**Start the recording, then the pane, then attach.** Three terminals is easiest; one is enough.

```sh
# terminal 1 — the node
marion run claude --pane --prompt "walk the tree with me" --timeout 900
# it prints: marion: attach with `marion attach <agent-id>`

# terminal 2 — the operator, recorded
asciinema rec docs/recordings/m3-c1-<date>.cast --command "marion attach <agent-id>"
```

*(2026-09-05: the native facade is live, so the session is to be recorded through it instead —
`asciinema rec docs/recordings/m3-c1-<date>.cast --command "marion claude"` — one terminal, the
harness's own TUI on the operator's terminal through the relay, the node in `marion tree` from
another. The `run --pane` + `attach` pair above still works and remains the shape step 5's
granted `claude-impl` type needs; the facade passes the operator's own flags through opaquely and
compiles no grant of its own, so what step 5 reads on a facade session is the harness's ordinary
permission behaviour — record it either way.)*

Ten minutes on the clock from the attach, with a real model behind it — `--pane` without
`--canned` uses the login the operator already has, which is the point: the criterion is about a
session a person would actually have.

**What to watch for, one line per clause.** Each is a thing to *do* and a thing to *see*; note
either in the recording's companion file.

1. **Trust dialog on the main screen.** A fresh checkout gets it. Before answering, confirm it is
   painted where a shell prompt would be — not on a screen that wiped what was above it.
2. **Alt-screen switch.** Answer the dialog. Whatever the harness does next, the pane must keep
   painting. **Note which happens**: 2.1.220 entered `?1049h` here, 2.1.225 does not enter one at
   all, and if the version under test switches back this is where it shows.
3. **Resize.** Drag the window edge at least twice, once narrower and once wider, mid-turn if
   possible. Nothing should be clipped, doubled, or frozen at the old width, and the terminal's
   scrollback from *before* the attach must still be there afterwards.
4. **Mouse.** Click in the pane, drag a selection, scroll the wheel. **If the harness enables no
   tracking mode, the correct behaviour is the operator's own terminal selection** — marion
   mirrors what the node asked for and invents nothing, so "the mouse does nothing in the TUI" is a
   pass on 2.1.225 and a fail on a version that asks for `?1000h`.
5. **Permission prompt.** Run this clause under a **granted** type, which is a different command
   from the one above — `marion run claude` declares no tool at all and can only ever record an
   absence:

   ```sh
   marion run claude-impl --pane --repo <a git worktree> --prompt "" --timeout 900
   ```

   `--repo` must be under git and `--no-change-record` must **not** be passed: `no audit, no
   grant` is a gate, and a bare directory is refused by name. Before typing anything, confirm the
   launched argv carries `--tools Read,Write` — `ps -o args= -p <the node's pid>`, or the
   `Spawned` record in the journal. If it carries `--tools ""` you are on the orchestrator type
   and this clause cannot be exercised.

   Then, in order, and record what happens for **each** — the absence of a dialog is a reading, not
   a failure to perform the step:

   a. **Ask for a file write inside the worktree.** Read `--allowedTools` off the same argv first:
      it carries `Read,Write` beside marion's verbs, because §3.1's two axes come from one
      declaration. `--allowedTools` is pre-approval, so the expected reading here is **no dialog**.
   b. **Ask for a write to a path outside the worktree** (`$HOME/marion-c1-probe.txt` will do).
      Whether the harness treats "allowed tool, disallowed location" as a fresh question is
      **unmeasured** — that is what this probe is for. Delete the file afterwards if it appears.

   If a dialog renders in either case, the clause's own words are what to check: it must render
   **in the pane**, be answerable from the keyboard, and the answer must **take effect** (the file
   appears on approve, and does not on deny — check with `ls`, not with what the model says it
   did). Note also that marion records none of this: no `PermissionDenied`, no `permission_denials`
   entry, and the change record shows only what was written. This is the harness's own dialog, not
   marion's `permission/request` queue (§11 item 22); marion is deliberately not in this loop on a
   pane, and routing it through item 22 would replace an answerable question with a queue entry
   nothing can answer.

   If **neither** probe produces a dialog, record that — it is the finding, and it means the clause
   cannot be met through `marion run` as it stands, since no built-in type can put a tool in
   `--tools` without also putting it in `--allowedTools`.
6. **Detach and re-attach.** `^] d`, confirm the shell comes back on the **main** screen with its
   scrollback and its cursor, then `marion attach <agent-id>` again and confirm the node is where
   it was left.

**Where it lands.** `docs/recordings/m3-c1-<date>.cast`, with a companion
`docs/recordings/m3-c1-<date>.md` naming the harness version, the terminal emulator, the geometry,
and one line per clause above saying what was seen.

**Then it is C3's work too.** §9 intends the recorded cast to be **promoted to a fixture** and
replayed under L4.5 — the same session, seen twice: once by a human deciding whether it looked
right, and once by a snapshot that fails when it stops looking that way. Promoting it means
dropping it beside `tests/fixtures/s2/`, adding a `fixtures::` constant, and adding a
`snapshot_test!` in `marion-term/tests/replay.rs`. Do not promote a recording whose companion file
says a clause failed.

- **M4 [done]** — N→1 fan-in: a Codex root spawning two Claude children concurrently. **Audited
  clause by clause against the code and the test on 2026-08-08, and every clause §9 names is met.**
  One test measures six of the seven —
  `marion-supervisor/tests/m4_fan_in.rs::a_real_codex_root_runs_two_real_claude_children_concurrently_and_receives_both_contracts`,
  a real `codex` 0.147.0 root and two real `claude` 2.1.226 children against the canned provider —
  and the seventh is a reading of marion's source, recorded below against the path it was read on.
  Clause 4 was **not** met when the test landed; it is now, and the paragraph on it says what was
  wrong.

  1. **A real `codex` root.** `on_path("codex")` is not a presence check — it runs `--version` and
     panics unless the output matches `PINNED_HARNESSES`, so the root's binary is gated at the
     pinned version even though its own `Spawned` records `harness_version: "unknown"` (a root has
     no contract, so nothing on that path shells out for a version). *That a real codex spoke* is
     asserted separately from the wire: a recorded `responses` request carrying code mode's
     `{name, namespace}` dispatch form, which S6 measured and no other harness here emits.
  2. **Two real `claude` children of that root.** The `SpawnIntent` records whose `parent_id` is the
     root are counted (exactly two) and each is checked for `harness: "claude-code"`,
     `agent_type: "claude-impl"`, `depth: 1`; then each child's `Spawned.harness_version` — marion's
     own reading of the binary it launched, taken for §6.7's `TaskContract.child.version` — is
     checked against the same `PINNED_HARNESSES` table, not a second list.
  3. **Concurrently, and with no duration compared against any threshold.** Two independent facts,
     and each was separately shown to kill a marion that runs the children one after the other.
     First, a rendezvous inside the canned provider answers neither child until both have asked for
     a turn, and `Rendezvous::expired()` is read as a *reading* — a serialized marion leaves the
     first child waiting alone and fails with a sentence rather than hanging. Second, the journal's
     total order is its byte order, so line indices give Allen overlap: each child's `SpawnIntent`
     precedes the *other* child's `Exited`. Fact 2 is marion's own account, written by the code
     under test, and is what the clause is graded on.
  4. **Both reported through marion's `report` over MCP, into their own contracts.** Per contract:
     `ExitStatus::Ok` (an `Unreported` status is what an absent report produces),
     `narrative_synthesized == false`, and `requester == ` the codex root. **The attribution half
     was measured and was not being measured.** The two assertions that landed with the test were
     each over a *set* — the narratives present, the changed paths present — and a marion that
     handed each child's report to its sibling satisfies both, since only the pairing is crossed.
     Run as a mutation, that defect **passed**. Closed on 2026-08-08 by asserting the pairing
     *inside* one contract against `changed_paths`, the one field no report can influence (§6.7
     sources it from a git diff of that child's own worktree).
  5. **The root received both contracts, asserted on the request side.** Read out of the root's own
     next-turn transcript, not out of what marion says it replied: each `wait`'s
     `function_call_output` must not be a `<persisted-output>` stub, must deserialize to a
     `TaskContract`, and must equal the persisted `contracts/<task_id>.json` after both sides are
     normalized for §6.7's cap rules. Then the two must be *different* contracts, and the pair must
     be exactly the two task ids the children ran under.
  6. **No adapter-specific orchestration code — read from the source, not from a green test.** The
     fan-in path is harness-blind in non-test code: `handler.rs` (spawn/wait/collect), `background.rs`
     (the handle table), `courier.rs`, `bridge.rs`, `journal.rs` and the supervisor's `registry.rs`
     contain no `match`/`if` on `Harness` at all — `handler.rs`'s only harness token in 6 863 lines
     is `harness: intent.harness` copying a field into a journal record, and `bridge.rs`'s only one
     is string interpolation in `failure_line`, whose `match` is on `ExitStatus`. The
     `max_concurrent_children` gate (`agent_type.rs:362`) reads the caller's type and names no
     harness. **Per-harness *compilation* is expected and is not this clause**: `adapter_for`
     (`marion-harness/src/adapter.rs:1408`) is the workspace's one per-harness dispatch table and it
     returns only a `Box<dyn HarnessAdapter>`; every supervisor call site consumes it through the
     trait. The three-way drive forks (`run.rs:1519`, `root.rs:1085`) branch on `LaunchPath` derived
     from `adapter.surfaces()` — codex, gemini and opencode all traverse the same `LaunchOnly` arm —
     which is a control plane, not a name. **Two things are recorded rather than waved past.**
     `harness.rs:69` `writes_without_a_declaration` is a genuine four-arm match on the enum, and it
     reaches a concurrency gate through `agent_type.rs:197` and `run.rs:1220`; it is off M4's path,
     because that gate is in the `Isolation::SharedCwd` arm and M4's children are worktree-isolated,
     but it is the sharpest thing an adversarial reader will find. And `spawn.rs:1077` hardcodes
     `Harness::Codex` in `build_contract` as a placeholder that `run.rs:1649` unconditionally
     overwrites from the adapter — not a branch today, a silently wrong audit record if that
     overwrite is ever removed.
  7. **Fan-in aggregates rather than serializing.** Distinct from clause 3 and measuring a different
     defect: both `spawn`s answered with a **handle** — the sentence, and *not* something that
     deserializes as a `TaskContract` — which is what let the second child start before the first
     was collected, and both `spawn`s precede both `wait`s in the root's own transcript. A `spawn`
     that returned the contract would have produced the same four turns and would have been the
     serialization the clause forbids. Note the division of labour honestly: clause 7's own
     assertions do **not** detect a marion that backgrounds correctly and then blocks anyway;
     clause 3's do.

  **The mutation table (2026-08-08), each entry a named failure and none a bare timeout.**
  *Children serialized* — `spawn { background: true }` awaits the contract before answering with its
  handle: killed at the `!rendezvous.expired()` assertion, and, with that assertion removed to test
  the other fact alone, killed again at the journal-overlap assertion (`3..=5` then `7..=9`, no
  overlap). *One child's contract dropped* — every `wait` resolves to the first handle: killed
  deserializing the second `wait`'s result, which was the already-collected sentence. *
  `max_concurrent_children` forced to 1*: killed at the child count (`left: 1, right: 2`).
  *A report attributed to the other child* — the two children's narratives swapped as they leave
  `ChildOutcome::from_stream`: **survived**, verified as a real swap (the contract whose diff is
  `src/m4_a.txt` carried child B's narrative) and now killed by clause 4's pairing assertion.

  **§11 item 31 at N=2, claimed no wider than it was measured.** Two children means two worktrees,
  and item 31's cross-process half is open. It is **not exercised here**: sampling `ps` through a
  run shows one `marion-supervisor serve --detached` daemon and three transient `marion-supervisor
  mcp` bridges, and only the daemon runs git — so both `make_worktree` calls are two threads of one
  process and are serialized by `spawn::repo_write_guard`, whose in-process half is exactly the
  half that is closed. So: **N=2 was safe in this test, on this machine, across the runs recorded
  here** — that is the whole claim. It is not "N=2 is safe": S17's own N=2 row is 1 failing
  repetition in 3 at 600 iterations each (~1 in 1 800 operations) for *unguarded concurrent
  processes*, and this test performs one worktree pair once, so it is orders of magnitude short of
  witnessing anything either way. Two `marion` processes on one repository remain unmeasured here
  and item 31 stays OPEN.

  **Verified.** `cargo test --workspace`: **1 188 passed, 0 failed, 0 ignored** (base 1 176 at
  `6813810`, re-derived rather than restated — the diff since adds 12 `#[test]` and removes none).
  `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` clean. No
  `marion`, `codex` or `claude` process outlived the run, `git worktree list` holds only the
  checkout, and the run's scratch tree is gone; the two directories left under `/tmp/mn-501` are
  another test's, from 03:05, and predate this work.
- **M5 [partial]** — ACP breadth, degrading per resolved capabilities. **Re-ruled 2026-08-08**,
  after the ACP adapter landed and S21 drove a real `opencode acp` child to a marion tool call.
  **Re-ruled again 2026-09-05**, after the ACP row became a baseline any ACP agent runs on, with
  the measured rows layered over it — the block immediately below; the 2026-08-08 ruling is kept
  under it, with the lines it superseded marked.

  **"Any ACP works" — what it means precisely, as of 2026-09-05.** An agent that speaks ACP wire
  v1 over stdio runs as a marion child with **zero per-agent code**: the agent type is
  `acp:<command> [args…]` (`marion_core::agent_type::ACP_COMMAND_PREFIX`; e.g. `acp:goose acp`,
  `acp:npx -y some-acp-agent`), and the launch is the operator's argv verbatim, nothing of marion's
  added. The protocol supplies what a row supplies elsewhere, and marion now leans on exactly that:
  1. **Identity** from `initialize` (`AgentHandshake`; §3.3 stage two narrows the ceiling per agent,
     as before).
  2. **The bridge** declared through `session/new`'s `mcpServers` — the protocol-standard channel,
     `McpRoute::Session("mcpServers")`, verified before a byte is sent.
  3. **The agent's spelling of marion's verbs discovered at runtime**, not looked up.
     `acp::Reading::Generic` recognises the `<server> <verb>` pair off the agent's own `tool_call`
     frames — the title split on any non-alphanumeric run with an optional leading `mcp`, or the
     structured `{server, tool}` `rawInput` — and reads the arguments where that shape puts them.
     Measured against every verbatim capture in which a real agent called `report`: `marion_report`
     (S21 opencode), `mcp__marion__report` and `mcp.marion.report` (S22 shims), `marion-report`
     (S28 copilot) — four agents, four spellings, one pair
     (`acp::tests::the_generic_reading_finds_every_measured_agents_report`). It is a *pair* match,
     not a prefix match, so S22's phantom `mcp__marion__startup` is named `startup` and never taken
     for `report`. `marion_tool_name` on the generic path answers `acp::GENERIC_SPELLING`
     (`marion_report`) — a baseline default that on this row reaches no model, since ACP has no
     availability axis and the prompt rides `session/prompt`; the reader does not depend on it.
  4. **Resume through `session/load`** where `initialize` advertised `loadSession`: a `LaunchSpec`
     carrying a session compiles the agent's argv unchanged and a `session/load` naming the session
     with the same `mcpServers` block; the driver sends it only to an agent that advertised the
     capability (`AcpChildError::NoLoadSession`, by the capability's name and the agent's,
     **before** any frame goes out) and prompts on the session the load named, since the load's
     answer carries no id. Proven against fake `sh` agents in `acp_child::tests`; the adapter half
     on both a refinement row and a generic command.
  5. **Refused by name only what the protocol cannot express**: no agent named (§6.4), a selector
     with no program (`acp::BindError::NoProgram`), `Auth::Canned` for an agent with no measured
     recipe (ACP names no provider anywhere), a pane (no terminal), and a resume on an agent without
     `loadSession`. **Gone:** the "marion has never heard of that agent" refusal — an unknown agent
     is the baseline, not an error — and the s14 spelling gate on `session_declaration`, which
     guarded a compiled spelling this row never compiles.

  **The refinement layer.** `acp::AGENTS` is now a table of *refinements* looked up by id through
  `acp::Binding::resolve`, with the generic command as the fallback (`acp::Binding`; a first word
  that is a row's id binds the row and its measured argv, anything else is a whitespace-split
  command). What a row adds is exactly what was measured: a pinned `Reading::Measured` spelling
  (five rows, four measured; `gemini`'s stays `None` and reads generically), a canned recipe
  (`opencode` only), and — new — a **declaration quirk**: `acp::Declaration::Argv` on the
  `copilot` row, because S28 measured copilot 1.0.83 accepting and *ignoring* `session/new`'s
  `mcpServers` (two sessions, zero frames reached the declared server, the model said the tool
  *"is not available in this session"*) while honouring its own `--additional-mcp-config`. The row
  declares there — the same document `copilot::mcp_config_json` writes for `copilot -p` — leaves the
  session block empty so a version that honours the protocol channel does not start two bridges,
  and reports `McpRoute::Argv("--additional-mcp-config")` verified against argv. The `acp-opencode`
  built-in is unchanged and binds the `opencode` row; `acp:copilot` binds the copilot row while
  `acp:copilot --acp` is a *command* and gets the protocol channel — which on 1.0.83 is the
  difference between a report and silence, and is why the row exists.

  **Evidence, RED first.**
  - `tests/acp_child.rs::a_previously_unknown_acp_agent_reaches_marions_bridge_through_the_generic_path`
    — `tests/fixtures/acp/fake_acp_agent.py`, an ACP agent in no row, spelling its mirrored call
    `marion/report` (a *fifth* spelling), spawned through `run_spawn` as `acp:python3 <path>`. RED
    on the unchanged tree: `unknown agent type acp:python3 …/fake_acp_agent.py`. GREEN: it starts
    marion's real bridge off `session/new`, calls `report`, and the contract carries its narrative
    (read by the generic reader) and git's `changed_paths`. $0.00, in the default suite.
  - `tests/acp_child.rs::an_acp_agent_runs_as_a_child_and_reports_through_marions_bridge` — the
    canned `opencode acp` child, unchanged and still green through the refinement row.
  - `tests/acp_child.rs::a_real_copilot_acp_child_reports_through_the_argv_declared_bridge`
    (`#[ignore]`, spends the operator's Copilot tokens) — **run 2026-09-05, passed in 9.6 s**: a
    real `copilot --acp` 1.0.83 child through `run_spawn` on the operator's login wrote the file,
    called `marion-report` through the argv-declared bridge, and the contract carries the narrative
    and git's `changed_paths`.
  - `acp_child::tests::{a_resumed_turn_loads_the_named_session_and_prompts_on_it,
    a_resume_is_refused_by_name_when_the_agent_does_not_advertise_load_session}` — RED on the
    unchanged driver with `Declaration { what: "its method is `session/load`" }`.
  - `doctor::tests::an_acp_command_gets_its_own_doctor_row_keyed_on_its_own_handshake` —
    `marion-supervisor doctor --capabilities --harness acp --acp-command "<cmd>"` adds one row
    beside the table's five, keyed on the agent's own `initialize` (`fake-acp-agent 0.1.0`).
  - `tests/fixtures/s28/` — verbatim: copilot's session that ignored the declaration, copilot's
    session that called `marion-report` plus the MCP wire behind it, and qwen's, goose's and
    gemini's `session/new` refusals.

  **§9's clause 1, re-ruled: MET.** *"At least two ACP agents beyond the day-one set run as
  children through the single ACP adapter"* — `opencode acp` (canned, default suite) and
  `copilot --acp` (live, `--ignored`, run 2026-09-05) both run as children through `AcpAdapter`
  via `run_spawn` → `run_acp_child`, each reporting through marion's bridge with the report read
  out of its transcript. Stated with its caveat: copilot needs a refinement row, because its
  protocol channel is broken on 1.0.83 — the *adapter* is single, the declaration channel is not.
  Clause 2 stays **MET** (five table rows plus any `--acp-command`). Clause 3 stays **NOT MET**
  for the one reason below — `NodeSummary` names a harness, not the agent's handshake identity — and
  it is now the *only* open clause, so **the marker stays `[partial]` on clause 3 alone**.

  **Known protocol-level limits, so nobody reads "any ACP works" as more than it is.**
  - *Vendor-side `session/new` refusals are the agent's, in its own words, and no adapter routes
    around them.* S28: `qwen --acp` 0.23.0 (`-32000`, *"Authentication required: Use Qwen Code CLI
    to authenticate first"*), `goose acp` 1.49.0 (`-32603`, *"Configuration value not found:
    GOOSE_PROVIDER"*), `gemini --acp` 0.53.0 (`-32000`, Gemini Code Assist ineligibility, as S20).
    Every one names its `authMethods` in `initialize`; §6.4 says marion may not pick one. All three
    *would* run through the generic path once the operator logs in / configures a provider — that
    is a claim about the path, not a measurement of those agents reaching a tool call.
  - *No provider channel.* ACP names no base URL or credential, so the generic path is
    `Auth::Inherited` only; `Auth::Canned` needs a measured per-agent recipe (opencode's document).
  - *No tool-availability axis.* Nothing in `initialize` or `session/new` narrows the agent's own
    tools; `compiled_permissions` records `NO_TOOL_AVAILABILITY_SURFACE`, `tool_name` refuses every
    marion verb, and the driver answers `session/request_permission` permissively. §11 item 24's
    two axes do not exist on this row.
  - *Resume is wired at the adapter and driver, and unreachable from `node/resume` today.*
    `node/resume` is root-only, no ACP node is a root, and the ACP row journals no session id (it
    has no `StreamGrammar` row; the id is in the `session/new` result frame). Both are marion's
    plumbing, not the protocol's.
  - *An agent that honours neither `session/new`'s `mcpServers` nor any argv channel marion knows
    reaches no bridge*, and the run reports nothing rather than failing — the s14 shape. The
    generic path cannot detect this before the turn; `marion doctor --acp-command` with
    `--adapter` is the way to find out, and a refinement row is the fix once measured.
  - *Whitespace-split selectors.* `acp:<command>` has no quoting grammar; an argument that needs a
    space needs a wrapper script (`BindError::NoProgram`'s text says so).

  **Verified 2026-09-05** at the series' head: `cargo test -p marion-harness` **254 passed** (was
  247 at the base, +7 unit tests: 4 in `acp.rs`, 3 in `adapter.rs`, with the ACP refusal test
  rewritten as `an_acp_launch_is_refused_only_for_what_the_protocol_cannot_express`);
  `cargo test -p marion-supervisor --test acp_child` **3 passed, 1 ignored** (the live copilot
  test, run once by hand as recorded above); `cargo test -p marion-supervisor --lib doctor::` **19
  passed** (+1) and `--lib acp_child::tests` **8 passed** (+2); `cargo test -p marion-core --lib
  agent_type` **18 passed** (+1). `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --all --check` clean. Rebased onto `48e908f` and re-run: the same counts, and
  `cargo test -p marion-supervisor --test harness_matrix` **5 passed** (its claude row was red at
  the series' original base only because the installed `claude` is 2.1.261 and that base's
  `PINNED_HARNESSES` did not yet admit it; `91e3822` does).

  **Read the marker precisely (2026-08-08 ruling, superseded where struck): one of §9's three
  clauses is met, and the marker does not move.**
  `[partial]` still names machinery rather than criteria. What changed on 2026-08-08 is *why* the
  other two are not met, and in both cases the reason narrowed rather than the status moving:
  clause 1 was two gaps, one of them marion's, and **that code gap is closed** — what remains is
  the operator/vendor one, which no marion code reaches. Clause 3 was blocked by a standing
  decision that `marion-tui` would be one full-screen pane; **that decision is reversed and the
  greying is built, doctor-sourced and measured** — what remains is that a node summary names a
  harness rather than an ACP agent, which is downstream of clause 1. Both are ruled below.

  | §9's clause | status | why |
  | --- | --- | --- |
  | *"at least two ACP agents beyond the day-one set run as children through the single ACP adapter"* | ~~**NOT MET** — but now for **one** reason, not two~~ **MET 2026-09-05** — see the re-ruling above (`opencode acp` canned + `copilot --acp` live) | **(a) closed.** `Harness::Acp` and `AcpAdapter` exist; a real `opencode acp` child runs end to end from the shipped `marion-supervisor doctor --adapter --harness acp` binary — spawn, `initialize`, `session/new`, `session/prompt` (`stopReason: end_turn`), `session/cancel`, clean termination, leak check. **(b) open.** Only one of the two installed agents can open a session at all: `gemini --acp` is refused `session/new` vendor-side. One agent runs; §9 asks for two |
  | *"with `marion doctor` reporting their differing capabilities"* | **MET** | `marion-supervisor doctor --capabilities --harness acp` prints **two rows** *(2026-09-05: five, one per row of `acp::AGENTS`, plus one per `--acp-command`)*, one per agent in `acp::AGENTS`, each keyed on `(acp, «the agent's own `agentInfo`», Typed(Acp)/StructuredUi/ProtocolEvents)` and each capability column produced by **that agent's own live `initialize`** (§3.3 stage two). `OpenCode 1.17.3` publishes `fork, resume, view, token_deltas, usage`; `gemini-cli 0.53.0` publishes `view, token_deltas, usage`. The difference is exactly `fork` and `resume`, which is what S20 measured, and it is asserted as that **named pair** rather than as "the two rows are unequal" |
  | *"and the UI greying out what they cannot do"* | **NOT MET** — but the blocker moved, and the half that was missing is built | **Was:** *"out of reach by a standing decision"* — there was no tree UI, and `marion-tui` was deliberately one full-screen pane. **That decision is reversed as of 2026-08-08**: `marion tree` exists, is reachable from the shipped binary (`marion --help` lists it; `crates/marion-supervisor/src/bin/marion.rs`'s dispatch calls `tree_main`), renders §5.6's tree pane beside a content pane, and greys every capability a node's harness cannot do on its surfaces. **What is not met is the ACP-specific half**, and it is downstream of clause 1 — see the two paragraphs under this table |

  **Clause 3, ruled precisely: the greying is built and measured; the clause is still not met, for
  one named reason.**

  What exists. `marion tree` draws every node `tree/subscribe` reports and, along the bottom, all
  ten of §3.3's capabilities for the selected node — the ones its harness cannot do on its surfaces
  in `DarkGray` + `CROSSED_OUT`, the rest plain *(2026-09-05: the strip is labelled `caps:`, an
  absent capability is drawn dim rather than struck through — it is unmeasured, not cannot — and
  an offered one bold; `tree.rs::a_greyed_action_differs_from_an_offered_one_only_in_style`)*.
  **The availability bit is not computed by the UI.**
  `marion_tui::tree` has no dependency that could reach a capability table and cannot name a
  harness; it receives an `Action { name, available }` and chooses a style.
  `marion_supervisor::tree::actions_for` is what decides, through
  **`doctor::capabilities_at` — the same function `marion doctor` builds its capability column
  from**, extracted from `probe_one` in the same change so there is one call site's worth of
  arithmetic and not two. The key is §3.3's whole key, and `NodeSummary` grew the two components it
  was missing to carry it: `harness_version` (from the journal's `Spawned`) and `pane` (from the
  supervisor's live pty map, the same source `node/attach` answers `PaneAttach` from).

  Measured by:
  - `marion_supervisor::tree::the_trees_greying_is_doctors_own_answer_at_every_key` — every harness
    × both surface roles × four versions, asserting the tree's offered set equals
    `doctor::capabilities_at`'s at **that exact key**. Keyed, not compared as sets: a set comparison
    would pass while two rows were swapped, which is a failure this repo has already shipped once.
    It also asserts the keys do not all produce the same answer, so it cannot pass for a tree that
    ignored its key.
  - `…::a_pane_node_is_not_offered_what_only_its_headless_key_can_do` — the named row in both
    directions. claude-code 2.1.223 headless offers `interrupt, permissions`; the same binary on a
    pane offers `interrupt` alone and draws `permissions` **greyed rather than absent**, because
    §3.4's `opaque` cannot carry a permission routing. That is §9's M1 sentence said about
    claude-code, and it is the exact defect the clause names.
  - `…::the_version_reaches_the_key_rather_than_being_dropped_on_the_way` — codex `resume` on
    0.145.0 vs 0.146.0 vs no version at all.
  - L4.5: `crates/marion-supervisor/tests/l45_tree.rs`, five reviewed snapshots over the production
    projection, whose payload includes a per-style-run census of the action strip.
  - `bin/marion.rs`'s `the_tree_screen_is_reachable_from_this_binary` — the usage text names the
    verb, `main` dispatches it, and `tree_main` really resolves a project instead of stubbing. Its
    own first version passed with the dispatch arm deleted, because `include_str!` fed it the test's
    own literals; that is recorded here rather than quietly fixed, because "the greying is measured"
    and "an operator can open the thing being greyed" are separate claims and this is the second one.

  **What is not met, and why it is downstream of clause 1.** §9's clause is about *"their"*
  capabilities — the two ACP agents'. `marion doctor` tells `OpenCode 1.17.3` from `gemini-cli
  0.53.0` because it performs each agent's own `initialize` and applies §3.3's **stage two**. A
  client cannot: `NodeSummary` names a **harness**, and `Harness::Acp` is one adapter over many
  agents (§5.2), so the tree publishes the protocol's unrefined static set and would offer `fork`
  and `resume` to a `gemini-cli` node that doctor's own row greys. That is stated in
  `doctor::capabilities_at`'s own doc rather than left to be discovered, and it is **narrowing in
  doctor, never widening** — the tree is never more permissive than stage one, only less specific
  than stage two.

  It is also not observable today: no ACP node can be in a tree at all. Clause 1 is not met, `acp`
  runs only under `doctor --adapter`, and nothing spawns an `Acp` node into the registry. Closing
  clause 3 therefore means giving a node summary the *agent* identity behind its harness, which is
  work that belongs with whatever first spawns one — i.e. with clause 1. **The marker does not
  move**, and the reason it does not is now one sentence about ACP rather than "there is no UI".
  *(2026-09-05: the first two sentences are false — clause 1 is met and `run_spawn` puts ACP
  children into the registry through `LaunchPath::Acp` — so an ACP node **is** in the tree today,
  greyed at the protocol's static ceiling. The third sentence is exactly what remains:
  `NodeSummary` still has no field for the agent's handshake identity.)*

  **What the adapter decided, and the argument for each.**
  - **Surfaces: `Typed(Acp)` / `StructuredUi` / `{ProtocolEvents}`** — §3.4's `headless` preset.
    *Typed* because ACP is a bidirectional session with `session/prompt`, `session/cancel` and
    `session/load`, so marion can address a turn rather than merely write bytes at one — the same
    vendor's binary one surface up from opencode's `LaunchOnly` row, which is why §3.3 keys on
    surfaces at all. *`StructuredUi`, not `NativePty`*, because marion owns no terminal here;
    `NativePty` would mint the node a `PtyWitness` and put it one call from `spawn_pty` (§11 item
    1). *`ProtocolEvents` alone*, because the frames are the record — there is no transcript file
    this adapter could know how to find, since the file is per-agent and the adapter is
    per-protocol. `pane_surfaces()` is `None` and a pane request is refused by name.
  - **The ceiling is `Typed` + `ProtocolEvents`, which permits all ten** — so on this row the
    ceiling limits nothing, and §3.3's stage two is doing the whole job. That is stated rather than
    glossed: `advertised(Acp, _)` claims **five**. `token_deltas` and `usage` are S21 measurements
    (per-token `agent_message_chunk`; a `usage_update` frame and a `usage` object on the prompt
    response). `fork`, `resume` and `view` state *the protocol's* shape precisely so the handshake
    can narrow them per agent — a `false` there would be final, because a meet cannot widen, and
    `opencode acp`'s advertised `fork` could then never be published. `steer`, `interrupt`,
    `permissions`, `elicitation` and `set_model` are all defined by ACP and **none has been driven
    against a live agent**, so all five stay `false` (§3.3's *degrade visibly*).
  - **`writes_without_a_declaration() == true`**, decided rather than copied from a neighbour.
    ACP has no tool-availability surface anywhere, so marion compiles no constraint; S21's session
    had `write`, `edit` and `bash` in scope with marion asking for nothing; and marion's own
    `initialize` hands the agent a further write channel (`fs.writeTextFile: true`). Independently,
    marion does not know *which agent* this is until the process exists, so any `false` would be a
    claim about an agent nobody had named.
  - **A fifth `McpRoute::Session("mcpServers")`.** `89b822d` declined the adapter partly because a
    fifth variant would sit in the declaration check *"with nothing behind it to check"*. **S21
    measured that clause false.** `session/new`'s `mcpServers` block is compiled by
    `HarnessAdapter::session_declaration` *before* a byte is sent, so it is checkable exactly when
    argv is — and `tests/fixtures/s21/opencode-acp-mcp.jsonl` is a real `opencode acp` starting the
    server marion declared, calling `initialize`, `tools/list` and then
    `tools/call {"name":"report"}` on it. The check is not "the array is non-empty": it requires an
    entry **named `marion` with a non-blank command**, because a declaration built for somebody
    else's server is a node with no marion bridge wearing a green check.

  **The measured tool mapping, and the gate around it** *(2026-08-08; the gate is gone as of
  2026-09-05 — see item 3 of the re-ruling)*. S21: marion declared an MCP server named
  `marion` offering `report`, and the model typed **`marion_report`** — visible in the
  `session/update` `tool_call` frame's `title`. The MCP `tools/call` the agent then made carries the
  **unprefixed** `report`, the same two-layer split S13 measured on `opencode run`. That is **one
  agent's** spelling, and `gemini --acp` has never reached a turn, so its spelling is unknown.
  ~~`AcpAdapter` therefore refuses to put marion's verbs in front of an agent whose spelling has not
  been measured — by name, quoting what is known — rather than reusing opencode's.~~ s14 was why:
  an unknown tool name is *silently ignored* by claude, gemini and opencode alike, so a guessed
  *compiled* name would buy a run that looks healthy, exits 0, and called nothing. What 2026-09-05
  found is that this row compiles no name at all — the agent presents the bridge's tools to its
  model under whatever name it likes — so the hazard was always on the *reading* side, and
  `Reading::Generic` reads the pair in any spelling.

  ~~Three~~ Two further refusals, each by name and each distinct: **no agent named** (§6.4 — marion
  may not pick one, and it does not fall back to the one agent it has measured), ~~**an agent marion
  has never heard of** (with the known ids listed)~~ *(2026-09-05: an unknown agent is the baseline;
  what is refused is a selector with no program)*, and **`Auth::Canned`** (ACP has no protocol-level
  way to point an agent at an endpoint; refused rather than launched at the operator's real provider
  while the contract records a canned one). And `tool_name` refuses **every** marion verb: ACP has
  no availability axis at all, so unlike codex and opencode there is not even a *"coarsest
  equivalent"* to answer with.

  **What runs live, and what does not.**
  - *Live, from a shipped binary:* `marion-supervisor doctor --capabilities --harness acp` (two
    real `initialize` handshakes *— five rows plus any `--acp-command` as of 2026-09-05*) and
    `--adapter --harness acp` (the full ACP micro-contract against `opencode acp`, including a real
    model turn). Both are exercised by `cargo test`.
  - *Against fixtures:* the frame readers — `marion_calls`, `parse_stream`, the tool-name mapping —
    are asserted against `tests/fixtures/s21/`, which is a verbatim capture rather than a
    hand-written frame.
  - ~~*Not at all:* **an ACP node is not spawnable through `marion run`.**~~ **FALSE since
    `acp_child.rs` landed, and re-checked 2026-09-05:** `run_spawn` derives `LaunchPath::Acp` from
    the type's surfaces and `run_acp_child` drives the session; the `acp-opencode` built-in and
    every `acp:<command>` type name `harness: acp`. What is still true is narrower: an ACP node is a
    **child** — `marion run` starts roots, and no ACP type is a root type.

  **The blocker, unchanged and not marion's** *(2026-08-08; superseded 2026-09-05 — the second
  real agent arrived as `copilot --acp`, and clause 1 is met above; gemini's own refusal is
  unchanged and is now one of S28's three vendor-side `session/new` refusals)*. `gemini --acp`
  completes `initialize` and then
  cannot open a session: `session/new` answers `-32000`, *"This client is no longer supported for
  Gemini Code Assist for individuals."* S20 reproduced it with no marion involved, and it is the
  same vendor-side block this file already records for bare `gemini -p` reaching the ACP surface
  too. **What would unblock it:** a second ACP agent that can open a session — a credential that
  makes `gemini --acp` eligible (`gemini-api-key`, a Vertex credential, or a gateway, none of which
  marion may choose, §6.4 — `marion doctor` now *names* all four of the ids that agent offers), or
  a third ACP agent installed. Simulating it with a shim would make the marker move and the
  criterion false: §9's word is *"real"*.

  **Verified.** `cargo test --workspace`: **1 247 passed, 0 failed, 0 ignored** across 61 suites.
  The base was **measured, not restated**: `e091b28` checked out into a separate worktree with its
  own `CARGO_TARGET_DIR` gives **1 228 passed, 0 failed** across the same 61 suites, so the diff is
  +19 — and the tree adds exactly nineteen `#[test]` and removes none (1 in `marion-core`, 10 in
  `acp.rs`, 1 in `caps.rs`, 5 in `adapter.rs`, 2 in `doctor.rs`).
  `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` clean.

  **Every new assertion was mutation-checked** — fifteen mutations, fifteen caught, and two of the
  fifteen only after the test that should have caught them was written: `advertised(Acp).fork →
  false` survived the first pass (no `marion-harness` test named the ACP capability row) and
  `marion_calls` losing its pairing survived a mis-applied patch rather than a weak test. Both are
  now caught in `marion-harness` alone. The two guarding the leak check are driven against
  `sh -c "trap '' INT; …"` rather than a real agent, because an agent that happens to honour SIGINT
  would exercise no escalation — and that test needed the shell to *announce* its trap, since
  without the wait the SIGINT raced the shell's startup and killed a process still under the
  default disposition, which is a test of nothing.

  **No leak.** `--adapter` spawns real ACP agents and they are long-lived stdio servers, so this was
  checked specifically: no `opencode`, `gemini`, `claude`, `codex` or `marion` process outlived the
  run (the one `opencode serve` on the box has been up since 2026-07-30), `git worktree list` holds
  only the checkout, and `/tmp` gained nothing — the two directories under `/tmp/mn-501` are another
  test's, from 03:05, and predate this work.

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
to this one. *(2026-09-05: the journal sentence is history — the journal exists, is the detached
supervisor's boot path, and a lost root is relaunched under its own id by `marion resume`. The
descendant-gating and `verification` sentences are still true at `b85ea30`:
`spawn.rs` writes `live_descendants_at_report: vec![]` and a `spawn` carrying `verification` is
refused by name.)*

### Not started — what a working M1 does *not* imply

**Read this before treating M1 [done] as a statement about marion.** M1 was built disposably on
purpose (design doc §9), so a working hop — and a met acceptance criterion — rests on very little.
None of the below is covered by any M1 criterion. **Re-checked line by line against the tree on
2026-08-08**; each entry says whether it moved. Where an entry says "unchanged", that is a verified
claim rather than an unrevised one — but note that the 2026-08-04 pass said the same thing about
lines that had *already* gone false, so the phrase is only as good as its date. On this pass, three
of the entries below moved: the verb list, `isolation`/`allow_concurrent_writes`, and the crate
grouping.

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
  reason than it had yesterday. *(Every clause of that sentence is history as of 2026-09-05:
  M2 is `[done]`, and the same-day paragraphs at the end of this bullet take a restart through
  `node/resume` and `marion resume` to a relaunched root.)*
  **Re-checked 2026-09-05 — the sentence is stale in one respect and true in the one that
  matters.** `Orphaned` marking has had its substrate since `restart.rs` (M2 criterion 3), and a
  client attaching after a restart is now told the truth about an orphan — `ReplayResumable`, never
  a live channel. Replay now folds a **second `Spawned` on a known `agent_id`** as a resume — the
  node returns to `Live` with the new pid and start identity, `ReplayedNode::spawn_generation`
  counts the lifetime, and the earlier records, contracts and intent are untouched
  (`a_second_spawned_after_an_orphan_marking_returns_the_node_to_live_with_the_new_pid_and_start_id`,
  RED at `left: Orphaned, right: Live` before the arm existed). `restart.rs` deliberately does not
  count a relaunch as a fate, so a resumed node whose second process is also lost is orphaned
  afresh on the next boot rather than silently. `RecordKind::SessionObserved { agent_id, harness,
  session_id }` now exists as its own additive record — separate from `Spawned` because the
  harness names its session in its first frame, after the barrier record is already on disk — and
  replay sets `ReplayedNode::harness_session` from it
  (`session_observed_round_trips_and_replays_onto_the_node`). **No producer writes it yet**: every
  node still replays with `harness_session: None`. What a restart still does **not** do is bring
  any node back — nothing journals a session, no `node/resume` exists, so nothing writes that
  second `Spawned` yet. *(Superseded the same day by the paragraphs that follow: the producer,
  the method, the handler arm and the CLI verb each landed in turn.)*
  **2026-09-05, later — the session id is grammar data.** `StreamGrammar::session: Option<SessionId
  { at: Where, path }>` names, per row, the unit that carries the harness's own session id and the
  pointer to it, and one engine function `grammar::session_id(row, frame)` reads it — no
  per-harness reader. Measured rows: claude `system`/`init` `.session_id` (s10), codex
  `thread.started` `.thread_id` (s7), gemini `init` `.session_id` (s12), opencode `.sessionID` on
  every frame (s13), copilot **only** on the terminal `result` frame `.sessionId` (s24 — no earlier
  frame names the session, so a copilot node killed mid-run has no id and its resume is refused
  honestly); ACP has no row. `each_harness_row_extracts_its_session_id_from_its_first_frame` is the
  test, RED with `no session_id in grammar` / `no field session` before the field existed. Still no
  producer journals it; that is the next commit.
  **2026-09-05, later still — `SessionObserved` has its producer.** `session_watch::SessionWatch`
  sits beside `events::EventSink` on both node paths (`run::run_spawn` for a child,
  `root::launch_inner` for a root) and journals the id the first time a frame names one, through
  `grammar::session_id` and nothing harness-specific. The `LaunchOnly` path — codex, gemini,
  opencode, copilot — had no live seam at all (`run_bounded` drained the pipe whole), so
  `Drain::start_with_lines` now forwards each complete stdout line to the caller's thread as it
  lands and `run_bounded_watched` takes an `on_line` hook; the capture is still returned whole
  (`stdout_lines_reach_the_hook_while_the_child_runs_and_the_capture_stays_whole`). A node lost
  mid-run is exactly the one a resume needs the id for, and it never reaches a capture.
  `journal_wiring::a_real_run_journals_the_harness_session_id_for_every_node` (codex root, opencode
  child, both `LaunchOnly`) was RED with `(codex) replays with no harness session; kinds were
  [SpawnIntent, RootGrantDecided, Spawned, SpawnIntent, Spawned, Exited, …]` and is GREEN with one
  `SessionObserved` per node. Still no `node/resume`, and a pane (pty) root emits no frames, so a
  pane node has no journaled session and its resume will be refused honestly.
  **2026-09-05 — `node/resume` is the sixteenth method.** `Method::ALL` moves from fifteen to
  sixteen (`the_method_list_is_fifteen` and `input.rs`'s inbound-table pin both updated, the second
  proving keystrokes were not what grew the list), with `NodeResumeParams { agent_id, prompt }` and
  `NodeResumeResult { agent_id, state, spawn_generation }`. The handler arm is the next commit; for
  now the method is specified on the wire and still answered `Unimplemented`.
  **2026-09-05 — the launch shape is recorded, so resume never infers it.** `SessionObserved`
  gains `pane: bool` (additive, `skip_serializing_if` so a headless record is byte-identical to
  older journals) and replay folds it onto `ReplayedNode::harness_pane`. A pane emits no
  `stream-json` and so names no session, meaning every resumable node is headless and the value is
  `false` today — recorded anyway so a pane node's resume stays a checked refusal rather than a
  silent headless relaunch. `SessionWatch::new` carries the shape from the launch path (a root's
  `RootPath::Terminal` is the pane; children are never paned).
  **2026-09-05 — `node/resume` brings a lost root back.** The handler arm is built: preflight in
  §7.2/§6.7 order (node exists; is a root at depth 0; is finished — `Orphaned`/`ReapedIdle`/exited,
  never live; named a session; and its recorded process is provably gone — `AliveAndOurs` killed
  first with the confirmed group kill `session/quit` uses, `CannotTell` refused, `Gone` proceeds),
  then `agent/spawn`'s own launch path (`handler::launch_root`, factored out of `spawn_root` so
  spawn and resume are one launcher) fed a `RootSpec` rebuilt from the node's journal with its own
  id and session on it. `RootSpec` gains `resume: Option<(AgentId, String)>` (id reused, session
  handed to the row's measured flag) and `prepare_watched` reuses the id rather than minting one;
  `run::Env` gains `project_root` so the relaunch runs in the served repo's working tree
  (`resumable_root_cwd` — the parent of `.git`; a linked worktree's cwd is not yet recovered and is
  the stated boundary). Tests on the `Owning` fixture:
  `resume_relaunches_an_orphan_into_its_own_node_id_through_agent_spawns_path` (same id,
  `spawn_generation` 2, a second real `Spawned` on the journal) and
  `resume_refuses_when_the_orphans_process_cannot_be_identified` (a live recorded pid with no
  identity is `CannotTell` → `Conflict`, nothing signalled or launched). `node/resume` no longer
  answers `Unimplemented`.
  **2026-09-05 — `marion resume` is the CLI verb.** `resume <agent-id> [--prompt] [--repo]
  [--state-dir] [--canned [--base-url]]` (bin/marion.rs `resume_main`/`parse_resume`, dispatched
  before the run parser). Unlike `attach`, it **starts a supervisor when none serves** — a resumed
  node's supervisor died with it, so there is deliberately none to dial and starting one, which
  boots over the on-disk journal, is the operation rather than a silent fallback (documented at the
  call site). It `detach::ensure_supervisor`s, sends `node/resume`, and on success hands off to
  `attach::run` (reusing `attach::Session::pump`) to watch the relaunched node; a resume that could
  not happen and had started the supervisor quits it again rather than leave one lingering.
  `attach_verb.rs::resume_is_dispatched_and_starts_a_supervisor_when_none_serves` is the test, RED
  with `usage: marion` (the verb fell through to the run parser) before the arm existed.
  **2026-09-05 — the whole arc is measured end to end.**
  `restart_resume.rs::a_node_resumes_into_the_same_id_after_its_supervisor_is_sigkilled_and_a_new_client_hears_its_next_turn`
  drives a **real codex** root through the canned provider to a parked, live state; SIGKILLs its
  detached supervisor (the codex process survives, in its own group); asserts `marion attach`
  refuses; runs `marion resume`; and asserts the root returns under its **own** id, `Live`,
  `spawn_generation` 2, a **new** pid with the pre-kill process killed first (the `AliveAndOurs`
  branch, exercised here for real), a journal contiguous from ordinal 0 across the restart, and a
  post-resume Responses request carrying the session's prior turns — the proof codex resumed its own
  session rather than starting fresh under the id. `#[ignore]` (real binary + detached supervisor);
  runs in ~2–5 s. Codex keeps its session rollout under `$CODEX_HOME` = the node's own agent dir,
  which nothing deletes on orphan/reap (only `run::cleanup` removes the `worktree`, never the agent
  dir), so the resume finds it. **This closes plan-restart-resume.md steps 1–8.**
- ~~**The registry.** Unchanged. `marion_core::registry::replay` exists as a pure unit and is
  exercised only by tests.~~ **Off this line 2026-09-05, and it had been false since M2:**
  `registry::replay` is the detached supervisor's boot path over the on-disk journal (M2
  criterion 2, `client_run.rs`), `watch.rs` reads through it on a live run, and `node/resume`
  rebuilds a `RootSpec` from what it replayed. It is a pure function; it is not a test-only one.
- **Descendant gating (§7.6) is not in code** — principle 11 above is specified, not enforced.
  Unchanged, and now visible in the artifact: `spawn.rs` writes `live_descendants_at_report:
  vec![]` on every contract, so the field a gate would read is a hardcoded empty list.
- **`verification` command execution, so a contract's `evidence` is always empty.** Unchanged, and
  as of `77557e3` a `spawn` carrying `verification` is now **refused by name** rather than accepted
  and dropped — the commands still never run, but a caller can no longer be told they did.
  Design §11 item 23.
- The `Stop` hook path. Unchanged — no production code references it.
- ~~`marion doctor --adapter`.~~ **Off this line 2026-08-08**, and it should have come off at
  `c7cd5b5`: `doctor` has not been a stub since, both of §8's modes run against the installed
  binaries, and the ACP rows added since spawn real agents. The sentence about the `main.rs` stub
  described a line that no longer existed. **`marion-term`, `marion-tui` and `marion-proto` came
  off this line earlier the same day** — all three are workspace members and all three have tests;
  grouping them with `doctor` had them inheriting a verdict that was only ever `doctor`'s.
- ~~`status` / `wait` / `list` appear in `--allowedTools` but are **not implemented**~~ —
  **CORRECTED 2026-08-08, and this entry was wrong in two different ways.** The bridge declares
  **five** tools, not two: `spawn`, `wait`, `status`, `list`, `report`
  (`crates/marion-supervisor/src/bridge.rs:254-395`). `wait` was **never** in this position — it
  landed with backgrounding, because a handle with no primitive to resolve it is the anti-pattern
  marion exists to delete — so the entry was already false when it said "unchanged". `status` and
  `list` landed this session. Design §9's `ntools=2` is a **measurement of a past run**, not a
  standing claim, and reading it as one is what kept this bullet alive: the count to quote now is
  five, and the inert-allowlist-entry argument no longer applies to any of the three.
- ~~**`isolation` and `background`**~~ — **CORRECTED 2026-08-08.** `background` is implemented, and
  `isolation` is now implemented for both values marion serves: `worktree` and §3.1's own default
  `shared-cwd`, the latter constructing `Workspace::SharedCwd` for the first time. `remote` stays
  refused by name and deliberately is not representable in `contract::Isolation`.
  `allow_concurrent_writes` is likewise no longer accepted-and-dropped — §6.6's occupancy table
  exists (`spawn::CwdClaim`), a second write-capable child in one cwd is refused naming the holder,
  and this parameter is what lifts that. `name` remains accepted-and-dropped, deliberately, for the
  reason design §11 item 23 gives. **What is still open** is the *cross-process* half of the
  occupancy guard, which is design §11 item 31's seam, not a new one.

### What is not built — RE-CHECKED against the tree 2026-08-08; four of six lines had gone false

The list below says what does not *exist*, as against the list above, which says what is not
*done*. It was last verified at HEAD `08b4fde` on 2026-08-05, and **the section had rotted into the
document's least trustworthy passage**: four of its six absences have been built since, three of
them by M2's own work, and a reader taking it at face value would have concluded that M2 could not
have moved — while M2 has been `[done]` since 2026-08-07. An absence list is only worth its
re-check date, so each line now carries its own.

- ~~**No unix socket, anywhere.**~~ **FALSE since M2.** `crates/marion-supervisor/src/socket.rs`
  holds §2's `UnixListener` in production code, and a detached supervisor serves it.
- ~~**`events.jsonl` is never written.**~~ **FALSE.** `crates/marion-supervisor/src/events.rs` is
  the writer, and `handler.rs`, `run.rs` and `mcp.rs` all reach it.
- ~~**No `marion-tui`, `marion-term` or `marion-proto` crate.**~~ **FALSE.** The workspace is eight
  members, all three among them (`Cargo.toml:3-12`).
- ~~**`marion doctor --adapter` does not exist.**~~ **FALSE since `c7cd5b5`**, and this line was
  already false when it was re-checked on 2026-08-08 — it was carried forward unread, which is the
  exact rot the section header complains about. `crates/marion-supervisor/src/doctor.rs` implements
  both of §8's modes, `main.rs` dispatches to them, and the ACP rows added since spawn real agents.
- ~~**No ACP `ControlPlane`, so an ACP node cannot be spawned through `marion run`.**~~ **FALSE,
  re-checked 2026-09-05.** `crates/marion-supervisor/src/acp_child.rs` drives `initialize`,
  `session/new` or `session/load`, and `session/prompt` for a spawned child, answering the agent's
  own requests mid-turn; `run_spawn` reaches it through `LaunchPath::Acp`; `acp-opencode` and every
  `acp:<command>` type name `harness: acp`. Still true, narrowly: ACP nodes are children, not roots.
- **`verification` execution is unimplemented, so `evidence` is always empty.** *Still true,
  re-checked 2026-08-08.* Since `77557e3` a `spawn` carrying it is refused rather than accepted and
  dropped, so a caller can no longer be told it ran.
- ~~**`pid` capture is unimplemented.**~~ **FALSE.** `run.rs:1472` journals `Spawned { pid:
  Some(pid) }` from the owner that has the child, and `root.rs` does the same for a root. The
  sentence "every production writer sets `None`" no longer describes any writer.

**Descendant gating (§7.6) is the absence that did not move**, and it is the one principle 11 calls
non-negotiable: `spawn.rs:1048` still writes `live_descendants_at_report: vec![]`, so the field a
gate would read is a hardcoded empty list. It is listed above under "not done" and repeated here
because it is the item most likely to be lost among five corrections.

What does exist: `marion run <agent-type> --prompt …` launches the root (and, since 2026-09-05,
`marion <harness> <its own flags>` runs a native TUI as a root through the relay, `marion resume
<agent-id>` relaunches a lost root, `marion attach` and `marion tree` view the fleet, and
`acp:<command>` names any ACP agent as a child type); contracts persist uncapped
to `<agent-dir>/contracts/<task_id>.json` with the capped copy returned; timeouts are enforced with
a descendant-pgid sweep; scope is checked both preventively (at spawn) and detectively (git-derived).
Mechanism for all of it: design doc §5–§7.

### The evidence base — what 2026-08-04 changed, and what it did not

*(Read the counts in this section as 2026-08-04's: "four harnesses" and "sixteen pairings" were
the whole set then. At `b85ea30` the set is eight terminal harnesses plus ACP — `harness_matrix`
has eight cells and `cross_product` fifty-seven — and the headline count of 570 tests is that
day's, not the tree's.)*

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
- Windows. The current hand-rolled PTY supports macOS and Linux only; a Windows
  ConPTY/`portable-pty` bridge remains deferred.
