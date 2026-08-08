# `protocol/` — one fixture per MCP revision marion claims

Unlike every other directory under `tests/fixtures/`, **these are not captures.** The `sNN`
directories record what a real harness did; these record what marion *promises*, one file per entry
in `bridge::SUPPORTED_PROTOCOL_VERSIONS`. They exist so that the claim is a reviewable artefact
rather than a string in a slice.

## Format

One file per version, named `<version>.jsonl`, two lines, matching the `{"dir", "frame"}` shape s6
uses so the two can be read side by side:

- `dir: "in"` — the `initialize` a client offering that version sends.
- `dir: "out"` — the answer marion must give it.

For a claimed version the answer is always the offer echoed back. That is the whole point of the
list: `negotiate_protocol_version` returns the newest supported version not newer than the offer,
so a version marion claims comes back unchanged, and one it does not gets clamped.

## The review trigger

`crates/marion-supervisor/tests/mcp_conformance.rs`'s
`every_claimed_protocol_version_has_a_fixture_and_is_echoed` enforces the correspondence **both
ways**: a claimed version with no fixture fails, and a fixture for a version no longer claimed fails
too. It also replays each fixture against the compiled binary, so a fixture cannot drift from
behaviour.

Before adding a version, work through the five-point diff in the doc comment on
`bridge::SUPPORTED_PROTOCOL_VERSIONS`. In short: confirm the `initialize` result, the `tools/list`
tool shape, the `tools/call` result shape, the id-less-frame rule, and the JSON-RPC error codes are
all unchanged from the newest version already listed. If any moved, the code moves first.

## Why `2024-11-05` is still here

It is the floor, and it is the only entry a real harness has been *measured* accepting after
offering something else: codex 0.146.0 offered `2025-06-18` and took `2024-11-05` five times over in
`../s6/mcp-server-frames.jsonl`. It is what an unplaceable or absent offer gets.

## Known gap, recorded deliberately

`2024-11-05` and `2025-03-26` permit a client to batch requests in a JSON array; `2025-06-18`
removed batching. marion reads one frame per line and answers a batch with `-32600` rather than a
batch of results. No measured client has ever sent one, both revisions only ever *permitted* it, and
the failure is a named error rather than silence. See the doc comment on
`bridge::SUPPORTED_PROTOCOL_VERSIONS`.

## What is not here

`2026-07-28`. It removes the `initialize`/`initialized` handshake these fixtures are made of, so it
cannot be added by writing a file — see the dated entry in the design doc.
