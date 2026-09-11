# Pre-design research transcript — SDK sessions, subagents, graph plans

*2026-07-31. Superseded; background only.* The raw question-and-answer notes that preceded
`docs/specs/2026-07-31-marion-design.md`. Kept verbatim for provenance: the design's session
model, its refusal to re-open a running session, and the "graph-plan" idea parked in
MILESTONES.md's out-of-scope list all start here. Nothing in it is a current claim, and the
harness facts it quotes were not version-stamped.

---

If you run something in Claude code sdk, (or other harness sdk’s for future reference), is it possible to load it into Claude code as well? As in, opening that current python run and interfering with it? 

Am trying to create cli-based sub agents and Claude -p, I am not sure if you can reopen/close it at will without interrupting runs (or if it’s even possible)

Synthesized SDK session persistence, streaming capabilities, and subagent architecture patterns
Synthesized SDK session persistence, streaming capabilities, and subagent architecture patterns

Short answer: yes after the run, no during it.

SDK runs and the interactive CLI share the same session store. Sessions are stored under ~/.claude/projects/<encoded-cwd>/*.jsonl (or $CLAUDE_CONFIG_DIR/projects/...), where <encoded-cwd> is the absolute working directory with every non-alphanumeric character replaced by -. So if you capture the session_id (it comes in the first SystemMessage with subtype="init"), you can later run claude --resume <id> from the same cwd and pick up the full transcript in the REPL. If resume returns a fresh session instead of the expected history, a mismatched cwd is the usual cause. Handoff between SDK and interactive CLI works fine — it's just a shared JSONL transcript keyed by cwd. 
Claude Code Docs
Claude Code Docs

What you can't do is attach to a live run. There's no daemon or multiplexer; the SDK spawns the CLI as a child process over stdio, and the transcript is single-writer. Opening the same session in a second process while the first is mid-turn will corrupt or clobber history rather than "join" it. If you want to branch off a session that's still in use, fork it — forking creates a new session starting from a copy of the original's history that then diverges. Attaching mid-run isn't supported; forking is the sanctioned way to touch a session you don't own. 
Claude Code Docs

If you need to interfere mid-run, you have to be the parent process. Use ClaudeSDKClient in streaming-input mode instead of one-shot query()/claude -p: it lets the agent run as a long-lived process that takes user input, handles interruptions, surfaces permission requests, and manages sessions. That gives you await client.interrupt(), mid-flight follow-up queries, hooks, and can_use_tool callbacks. For steerable subagents, hold an open ClaudeSDKClient rather than shelling out to claude -p repeatedly. 
Claude

Practical shape for your setup: one persistent client per subagent, store its session_id, expose interrupt() + query() to your orchestrator, and for read-only observability just tail the session JSONL (or --output-format stream-json) from another process — reading is safe, writing isn't. Own the process for control, tail the transcript for visibility, resume/fork for anything after the fact.

I want it to be 1 to 1 almost with the current subagent system, so is there some way possible to use the claudesdkclient to basically replicate subagents/agent teams easily while keeping all features?

Examined SDK parity with existing subagent system and implementation considerations
Examined SDK parity with existing subagent system and implementation considerations

You don't need to replicate it — the SDK exposes the same subagent system, not a lookalike. Pass agents={...} in ClaudeAgentOptions (works identically on ClaudeSDKClient), and Claude delegates through the same Agent tool the CLI uses. Each subagent is an AgentDefinition with description, prompt, and optional tools, disallowedTools, model, skills, memory, mcpServers, maxTurns, background, effort, and permissionMode. Same engine, same tool — you configure it rather than rebuild it. 
Claude Code Docs

For literal 1:1 with what you already have, keep your markdown. Filesystem agents in .claude/agents/ still load in the SDK, and programmatic definitions take precedence over filesystem agents with the same name. You'll need setting_sources set so filesystem settings load at all. Point the SDK at your existing .claude/agents/ dir and you get parity for free. 
Claude Code Docs

The gotchas worth knowing up front:

Include Agent in allowed_tools or invocations fall through to can_use_tool / get denied. 
Claude Code Docs
Python keeps camelCase for disallowedTools and mcpServers to match the wire format. 
Claude Code Docs
Since v2.1.198 subagents run in the background by default; background: true forces it regardless of what Claude requests. 
Claude Code Docs
Nesting depth defaults to three layers, tunable via CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH. 
Claude Code Docs
The tool was renamed Task → Agent in v2.1.63, and some fields still emit Task, so match both. 
Claude Code Docs

Most parity bugs are one of these five, not architectural.

For actual teams rather than one-shot delegation, two things matter. Subagents are resumable: the Agent tool result contains agentId: <id>, and resuming the same session plus naming that ID in the prompt restores its full transcript, tool calls included. And a subagent holding the SendMessage tool starts with a list of the other named agents in the session, so it knows who it can message. Persistent, addressable, resumable agents already exist — you don't need your own message bus. 
Claude Code Docs
Claude Code Docs

So the shape I'd build: one long-lived ClaudeSDKClient as orchestrator, agents defined programmatically (factory functions if you want runtime model/effort switching), your interrupt and observability layer on the client. If you ever coordinate dozens-to-hundreds of agents, the Workflow tool moves orchestration into a script outside the conversation context. One client, native subagents, Workflow only if you outgrow turn-by-turn delegation. 
Claude Code Docs

Validated architectural distinction between lifecycle and view parity approaches
Validated architectural distinction between lifecycle and view parity approaches

Right — you've spotted the real limit. The proxy shim gives you lifecycle parity (a row, a name, resume, interrupt) but not view parity. Click in and you see the shim's transcript: MCP tool calls with Codex's output flattened into result strings. Claude Code's renderer only understands its own transcript schema, so Codex's reasoning blocks, its native tool calls, its diffs all collapse into opaque text. The mismatch is schema-level, not cosmetic — no amount of shimming makes a foreign event stream render natively.

Which means your instinct to build the UI is correct, and there's substantial prior art for exactly the normalization layer. ACP (Agent Client Protocol) is an open JSON-RPC 2.0 standard that abstracts the events and outputs of AI agents into a common interface — essentially MCP but for agent-to-client/UI communication. Local agents run as editor subprocesses over stdio; remote agents over HTTP/WebSocket. The adapter ecosystem is already broad: Codex CLI, Gemini CLI, Claude Agent (via Zed's SDK adapter), Goose, OpenCode, Qwen Code, Cline, Kimi CLI, and dozens more. Don't invent the event IR — ACP already is one, with the adapters written. 
Phil Schmid + 2

The gap ACP leaves is precisely yours. It's a client↔agent protocol: one UI driving one agent. It has no notion of an agent spawning peers, of team topology, of an orchestrator routing between harnesses. So the honest split is: adopt ACP as your transport and event schema so any harness renders faithfully in your UI, then build the orchestration/team layer on top as the thing that's actually novel. ACP solves rendering-any-harness; all-to-all delegation is the unsolved part you'd be contributing.

Two things to design for early: capability negotiation (harnesses differ on interrupt, permission prompts, checkpointing — your UI needs to degrade gracefully rather than assume Claude Code's feature set), and an escape hatch for harness-specific payloads your renderer can special-case instead of forcing everything to lowest-common-denominator. Normalize the common 80%, but carry the vendor extensions rather than discarding them.

No I mean we just build a UI around it, but still: no way to attach to live running thing? Are you fully sure?

Reconsidered attach capabilities and identified streaming limitations
Reconsidered attach capabilities and identified streaming limitations

I was wrong earlier, and this is worth correcting properly: live attach absolutely exists. Background sessions run as worker processes parented to a supervisor daemon rather than to any user shell, and the CLI talks to that supervisor over a local IPC channel when you run claude --bg, claude agents, claude attach, and so on. The documented command surface includes claude agents, attach, logs, respawn, rm, and stop, plus claude daemon status, which reports supervisor reachability, pid, version, socket directory, and live session count. There is a real control plane, and detach/reattach without interrupting the run is a first-class feature — my earlier "no daemon or multiplexer" was flatly incorrect.

The catch is the shape of the attach channel, which matters a lot for your plan. Per a reverse-engineering writeup (unofficial, so verify): the worker gets a CLAUDE_BG_RENDEZVOUS_SOCK and carries lifecycle events as newline-delimited JSON — heartbeat, state, done, detach-request, repaint-done, with shutdown/repaint/attacher-caps/reply going the other way — while the PTY channel uses binary framing: 4-byte big-endian length, 1-byte frame kind, where kind 0 is raw PTY bytes and kind 1 is JSON control messages like hello, live, exit, resize, kill. Lifecycle is structured JSON, but the content channel is a PTY — attaching hands you rendered ANSI, not semantic events.

So for "reopen the TUI at will," you're done: background sessions plus claude attach is exactly that. For feeding your own UI, screen-scraping a PTY is the wrong primitive — you'd want the session JSONL plus hooks for semantics, or ACP if you want the same treatment across harnesses. Attach solves the terminal-reopening problem; it does not give you a clean event feed to render.

Two hazards if you build on the daemon: it's undocumented, and it's authenticated by a per-daemon control key. After an auto-update the new daemon generates a fresh control key, and workers started by the old daemon become permanently un-attachable for the rest of their lives. Treat the IPC as unstable and pin auto-updates on any machine running long-lived agents.





Current plan.md is pretty much linear. Would a better orchestration harness have a shared memory for the plan, create fresh subagents with the memory + relevant context for each run, and have a graph-based plan, with exact testable things for each to 'pass' along with self-modifications and add-ons to teh graph?

And a good visual so users can see exactly what the progress is without having to do a /btw what is going on?

Does this exist already? If not, how useful will this be.

API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment. If it persists, check https://status.claude.com.

API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment. If it persists, check https://status.claude.com.

API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment. If it persists, check https://status.claude.com.

Searched the web, ran a command, loaded tools
Searched the web, ran a command, loaded tools

No prior state in this session — no plan.md here, so I'm answering the design question directly rather than editing a file.

Does it exist? Piecewise yes, assembled no.

Every individual piece is shipping somewhere. Graph plans over typed shared state are mature — LangGraph, Temporal, Mastra, and Microsoft Agent Framework all do "node receives state, transforms it, returns state," with subgraphs as reusable units. Fresh-context-per-node is the standard orchestrator-worker pattern, where workers hold only what their step needs instead of the whole transcript. Per-node pass criteria show up as "validation gates" in pipeline patterns and as acceptance criteria in spec-driven tooling like GitHub Spec Kit. Visualization exists as LangGraph Studio, Mastra's workflow editor, and Langfuse/LangSmith traces. Nothing you described is unproven in isolation.

The two genuinely open parts are (a) runtime self-modification of the graph — the production survey I read explicitly lists "dynamic graph modification during execution" as unexplored, with the work still in papers rather than products — and (b) progress visualization that is user-facing and grounded in pass/fail state rather than developer-facing spans. The novelty isn't the graph; it's a graph whose nodes are contracts and whose edits are first-class, streamed events.

How useful, ranked honestly

Per-node executable exit criteria is the highest-value, cheapest piece by a wide margin. It converts "the agent said it finished" into "this command exits 0," which is what actually kills the dominant failure mode of long linear plans — silent drift where step 3 half-worked and steps 4-9 built on sand. Do this first even if you change nothing else.

Fresh subagent per node with scoped context is second. It fixes context rot and cross-step poisoning, and it's mostly free today via subagents. The cost is real, though: the orchestrator now has to author each child's context packet, and a bad packet is worse than a long transcript. The win is real but it moves work from the model to your context-assembly code.

Graph-over-linear is third, and smaller than it feels. Most plans are ~linear with a couple of fan-outs, so parallelism gains are modest. The actual payoff is re-entry: when node 4 fails, a DAG tells you exactly which downstream nodes are invalidated and which completed work survives. Buy the graph for dependency-aware retry, not for concurrency.

Self-modification is fourth and the one most likely to sink the project. An agent that can add nodes will add nodes — unbounded scope creep, non-termination, and a plan that no longer resembles what the user asked for. It needs a hard budget (node count, depth, token spend), a distinction between refining a node and adding a goal, and human approval for the latter. Treat graph edits as privileged operations with a quota, not as a capability.

The visual is high UX value and low technical risk, and the /btw what's going on pain is real. The trap: a progress view that reports node status from the agent's self-assessment is a prettier lie. It's only worth building on top of the exit-criteria layer, where "green" means a test ran. Make the visual a projection of test state, never of intent.

The parts that will actually hurt

Writing good exit tests is the hard problem, and models write optimistic ones that assert what they just did rather than what was required — you need the test authored at plan time, before implementation, and ideally frozen. Second, the graph becomes a second source of truth that drifts from the repo; it needs to be derivable/checkable against reality, not just appended to. Third, parallel nodes touching the same files means either worktree isolation per node or a serialized write phase. Every one of these is a place where the harness looks impressive in a demo and rots in week three.

Smallest useful version

Keep plan.md linear for now, but add to each step: an explicit verify: command, an explicit context: list of files/facts the child gets, and a depends_on: field. That's a DAG in disguise, executable, and readable — and it gives you the progress view for free because node state is just "did verify pass." Graph self-modification comes later, gated. Ship contracts and dependencies first; the graph and the self-editing fall out of it, and if they don't, you've lost nothing.

Want me to draft that schema plus a runner and a live HTML progress view you can point at a real plan?

Sources: Graph-Based Agent Workflow Orchestration in Production: The 2026 Landscape, From Agent Loops to Structured Graphs, Verified Multi-Agent Orchestration: A Plan-Execute Framework, Plan-over-Graph: Towards Parallelable LLM Agent Schedule, GitHub Spec Kit, awesome-harness-engineering

I mean I know there's already stuff like /goal, and you could give it a goal. But oftentimes, the model just fails on the proper testing. So maybe testing the testing infra at the start is also imperative? Claude code already has a DAG in disguise but reviewing over plans isn't linear though which is annoying to look over. And I feel like it's still easy for claude code to drift from the plan as new bugs appear, new issues might come along. So adding the editable graph (with the main goal put in sight) would be better?

Yes — and I'd sharpen it: the problem isn't that the model doesn't write tests, it's that a test which has never been observed to fail carries no information. "Tests pass" is compatible with zero tests collected, everything skipped, a stale build, or an assertion that restates what the model just typed. So verifying the test infra isn't a nice-to-have first step; it's the thing that makes every downstream green light mean anything. A green check on unvalidated test infra is worse than no check, because it stops you from looking.

Concretely, node 0 should do four cheap things: run the suite as-is and record the baseline count, run it twice to catch flakes, deliberately break one assertion and confirm the runner reports failure, and record how long it takes. Then per-node, the exit test must be shown red before the work and green after — and that's mechanically checkable, since you have the pre-change tree in git. If a node's test passes against the unmodified code, the test is wrong, not the code. Red-before-green as a recorded artifact, not a claim, is the single highest-leverage rule in the whole design.

On plan review: I'd push back slightly on framing it as linear-vs-graph. A graph of 30 nodes isn't easier to eyeball than a list of 30 steps — arguably worse. What actually makes review cheap is a plan diff: nodes collapsed by default, and a persistent "here's what changed since you last approved this" view. You currently have no way to see that Claude Code quietly added three steps and dropped one, and that's the real annoyance, not the topology. Ship the diff before the graph; the diff is where the drift becomes visible.

On drift itself, the editable graph is the container but not the mechanism. The fix is forcing a classification every time something new appears: this blocks the goal (→ new node, requires approval), this is incidental (→ append to a "found, not doing" list, no work), or this is inside the current node's scope (→ just fix it). Almost all drift is category two being silently promoted to category one, and a goal pinned in the corner is passive — it doesn't stop anything. Make the agent name the category out loud; that's what converts a visible goal into an enforced one.

One addition worth making explicit: keep an immutable goal node whose exit criterion is the user's original acceptance statement, re-run at the end, and which graph edits are structurally forbidden from removing or rewording. Otherwise a self-modifying plan will eventually redefine success to something it has achieved. The goal must be the one node the agent can't edit.

Want me to write the node-0 infra check plus the red-before-green enforcement as an actual runnable script and schema? That's the part where the idea either survives contact with a real repo or doesn't.