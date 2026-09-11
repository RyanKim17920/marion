# `acp/` — the ACP agent marion has no row for

**Unlike every `sNN` directory here, this is not a capture.** `fake_acp_agent.py` is a hand-written
stdio ACP agent, committed as a *test input* rather than as evidence of what a real harness did. It
exists so that marion's generic ACP path has a witness that is provably outside every measured row.

## What it is

A minimal agent speaking ACP wire v1 over stdio. Its own docstring states what it does and why:

> It speaks ACP wire v1 over stdio and does exactly what a real agent does with marion's bridge:
> starts every MCP server declared in `session/new`, handshakes with it (`initialize`,
> `notifications/initialized`, `tools/list`), and — on the first `session/prompt` — writes one file
> into the session's cwd and calls the bridge's `report` tool for real, mirroring the call as ACP
> `tool_call` / `tool_call_update` frames.
>
> The mirrored tool title is spelled `marion/report`: a **fifth** spelling, on purpose, one that no
> row in `acp::AGENTS` was measured to use.
>
> Runs nothing but the servers it is handed; no model, no network, no credential.

The four spellings it is deliberately *not* — `marion_report` (S21, `opencode acp`),
`mcp__marion__report` and `mcp.marion.report` (S22, the ACP Registry shims) and `marion-report`
(S28, copilot) — are the measured ones, each with its own fixture directory.

## Which harness / version produced it

None. It is source, not a recording, so there is no harness, version or capture date to stamp. The
protocol shapes it implements were measured by S20–S23 and S28; the agent itself was written for
the tests below.

## Which tests read it

Both reach it as `$CARGO_MANIFEST_DIR/../../tests/fixtures/acp/fake_acp_agent.py`, and both skip
themselves when `python3` is not on `PATH`:

- `crates/marion-supervisor/tests/acp_child.rs` —
  `a_previously_unknown_acp_agent_reaches_marions_bridge_through_the_generic_path`, which runs it as
  agent type `acp:python3 <path>` and asserts on two fields the agent cannot fabricate through its
  own report: the narrative marion's reader pulled out of the ACP transcript, and git's account of
  the changed paths.
- `crates/marion-supervisor/src/doctor.rs` —
  `an_acp_command_gets_its_own_doctor_row_keyed_on_its_own_handshake`, which proves the doctor row
  is keyed on what the agent said in `initialize` (`fake-acp-agent 0.1.0`) rather than on any row
  marion carries.

Because the whole point is an agent no table names, **keep its tool spelling and its `agentInfo`
out of `acp::AGENTS`**; adding either would silently convert both tests into table lookups.

## Redaction

`tests/fixtures/REVIEW.md` is the gate for everything under `tests/fixtures/`. This directory has
no row in its provenance table and needs none — nothing here was recorded from a machine, so there
is no host, identity or credential residue to scrub. Any *capture* added beside it would need a row.
