# S20 — can two ACP agents beyond the day-one set actually run here?

Run:

```
mkdir -p /tmp/acpprobe
python3 spikes/s20/acp_probe.py         gemini   -- gemini --acp
python3 spikes/s20/acp_session_probe.py gemini   -- gemini --acp
python3 spikes/s20/acp_probe.py         opencode -- opencode acp
python3 spikes/s20/acp_session_probe.py opencode -- opencode acp
python3 spikes/s20/acp_probe.py         control  -- codex --acp     # negative control
```

Measured 2026-08-08, macOS darwin 25.5.0, `gemini` 0.53.0, `opencode` 1.17.3, `codex` 0.147.0.

## Why this was measured

Design §9's M5 says *"At least two ACP agents beyond the day-one set run as children through the
single ACP adapter, with `marion doctor` reporting their differing capabilities"*. §5.2 fixes the
day-one set: *"Day-one adapters: claude-code, codex."* So M5 needs two agents that are neither, that
speak ACP, and whose capabilities **differ** — a doctor table where every row is identical does not
report differing capabilities.

Nothing in the repo had ever spoken ACP to anything. The question "is M5 completable on this
machine" was therefore unanswerable from the tree, and answering it by installing an agent and
assuming it works is exactly the shape that produced §12's retractions.

## What was found

**Two ACP agents beyond the day-one set are installed and both complete `initialize`.**

| | `gemini --acp` | `opencode acp` |
|---|---|---|
| `agentInfo.name` / `version` | `gemini-cli` 0.53.0 | `OpenCode` 1.17.3 |
| `protocolVersion` | 1 | 1 |
| `loadSession` | true | true |
| `promptCapabilities` | image, audio, embeddedContext | image, embeddedContext |
| `mcpCapabilities` | http, sse | http, sse |
| `sessionCapabilities` | **absent** | **`{close, fork, list, resume}`** |

Verbatim frames: `gemini-initialize.json`, `opencode-initialize.json`.

The `sessionCapabilities` row is the finding that matters. It is a real, measured difference on
exactly two of §3.3's ten fields — `fork` and `resume` — between two agents behind one adapter.
That is what a `marion doctor` capability table has to be able to say.

`audio` also differs, and deliberately has no `Capabilities` field: §3.3 lists ten fields and prompt
content types are not among them. Recording the difference is not the same as claiming a capability
for it.

## And the blocker

**`gemini --acp` cannot open a session on this machine, so it cannot run as a child.**
`session/new` after a successful `initialize` answers:

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"This client is no longer supported for
Gemini Code Assist for individuals. To continue using Gemini, please migrate to the Antigravity
suite of products: https://antigravity.google"}}
```

This is the **same vendor-side block** `MILESTONES.md` already records for bare `gemini -p`
(`IneligibleTierError`, `reasonCode: UNSUPPORTED_CLIENT`, exit 55) reaching the ACP surface too, and
it is reproduced here with no marion involved. It is not an adapter bug and no adapter can route
around it: unblocking needs a `gemini-api-key`, a Vertex credential, or a gateway, none of which
marion may choose for the operator (design §6.4).

`opencode acp` gets all the way through: `session/new` returns a real `sessionId`
(`ses_01f0e32c2ffe…`) and immediately pushes a `session/update` notification. It is fully live.

So the count of ACP agents beyond the day-one set that can **run as children here is one, not two**,
and M5's first clause is blocked on the operator supplying a credential or a third agent, not on
code marion has not written.

## Negative control

`codex --acp` exits **2** with `error: unexpected argument '--acp' found` and writes nothing to
stdout. The probe therefore distinguishes an ACP agent from a non-ACP one by observation rather
than by the absence of an answer, so a green row above is the handshake happening and not the probe
failing to notice that it did not.
