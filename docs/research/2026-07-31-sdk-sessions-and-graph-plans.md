# Pre-design research notes — SDK sessions, subagents, graph plans

*2026-07-31. Superseded; background only.* The conclusions of the question-and-answer research
that preceded `docs/specs/2026-07-31-marion-design.md`, condensed from the original transcript.
The design's session model, its refusal to re-open a running session, and the "graph-plan" idea
parked in MILESTONES.md's out-of-scope list all start here. Nothing in it is a current claim, and
the harness facts it records were not version-stamped; MILESTONES.md holds the measured ones.

## Sessions: resume after, never write during

- A Claude Code SDK run and the interactive CLI share one session store
  (`~/.claude/projects/<encoded-cwd>/*.jsonl`, or under `$CLAUDE_CONFIG_DIR`). With the
  `session_id` from the first `init` message, `claude --resume <id>` from the same working
  directory picks the transcript up in the REPL. A mismatched cwd is the usual reason a resume
  comes back empty.
- The transcript is single-writer. Opening the same session in a second process mid-turn clobbers
  history rather than joining it; forking is the sanctioned way to branch a session you do not
  own.
- To steer a run while it is live you must be its parent: hold a long-lived streaming client
  (interrupt, follow-up input, permission callbacks, hooks) instead of shelling out to `claude -p`
  repeatedly. Reading the JSONL or `--output-format stream-json` from another process is safe;
  writing is not.

## Subagents: configure the native system, do not rebuild it

- The SDK exposes the same subagent mechanism as the CLI (the `Agent` tool, programmatic agent
  definitions, filesystem agents under `.claude/agents/` when setting sources are loaded).
  Parity problems are usually configuration: the tool missing from the allowed list, the
  `Task` → `Agent` rename, background-by-default, and the nesting-depth limit.
- Subagents are resumable by id and addressable by name, so a persistent team does not need its
  own message bus — within one vendor.

## Cross-harness views need a common event schema

- A proxy that wraps a foreign harness as a Claude subagent gives lifecycle parity (a row, a
  name, resume, interrupt) but not view parity: the other harness's reasoning, tool calls and
  diffs collapse into opaque result text, because the renderer only understands its own schema.
- The Agent Client Protocol (ACP) already normalises one client driving one agent, with adapters
  for many harnesses. It has no notion of agents spawning peers or of a team topology; that
  orchestration layer is the gap marion targets.
- Design for capability negotiation from the start (harnesses differ on interrupt, permissions,
  checkpointing) and carry vendor-specific payloads instead of flattening to the lowest common
  denominator.

## Live attach exists, but as a terminal

- An early answer that there was no daemon was wrong. Claude Code has background sessions under a
  supervisor process with `attach`/`agents`/`logs`/`stop` commands, so detaching and reattaching
  a terminal without interrupting the run is supported.
- An unofficial reverse-engineering account (unverified) describes the attach channel as JSON
  lifecycle events plus a framed raw-PTY stream: attaching gives rendered ANSI, not semantic
  events. Semantics still come from the session JSONL, hooks, or ACP. The IPC is undocumented and
  keyed per daemon, so treat it as unstable and pin auto-updates on machines running long-lived
  agents.

## Graph plans: contracts before topology

Every piece (graph workflows over shared state, fresh context per worker, validation gates,
workflow visualisers) ships somewhere; runtime self-modification of the graph and user-facing
progress grounded in pass/fail state were the open parts. Ranked by value:

1. **Per-node executable exit criteria** — "this command exits 0" instead of "the agent said it
   finished". Highest value, cheapest.
2. **Fresh subagent per node with a scoped context packet** — fixes context rot, at the cost of
   authoring good packets.
3. **Graph over list** — the payoff is dependency-aware retry (which completed work survives a
   failure), not concurrency.
4. **Self-modification** — the most likely to sink the project; needs a hard budget, a
   distinction between refining a node and adding a goal, and approval for the latter.
5. **Progress view** — worth it only as a projection of test state, never of the agent's
   self-assessment.

Rules that came out of the follow-up:

- **Validate the test infrastructure first.** A test never observed to fail carries no
  information. Node 0 records the baseline count, runs twice for flakes, breaks one assertion to
  confirm the runner reports failure, and times the suite.
- **Red before green, recorded.** Each node's exit test is shown failing on the pre-change tree
  and passing after; a test that passes on unmodified code is wrong.
- **Review a plan diff, not a graph.** What makes review cheap is seeing what changed since the
  last approval.
- **Classify every new finding:** blocks the goal (new node, needs approval), incidental (a
  "found, not doing" list), or in scope (fix it). Most drift is the second silently promoted to
  the first.
- **An immutable goal node** whose exit criterion is the user's original acceptance statement,
  which graph edits cannot remove or reword.

Smallest useful version: keep the plan a list, and give each step `verify:`, `context:` and
`depends_on:` fields — a DAG in disguise whose progress view is just "did verify pass".
