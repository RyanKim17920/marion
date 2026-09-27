# S33 — every ACP agent that installs without an account, probed to `session/new`

Measured 2026-09-27, macOS darwin 25.5.0. Each file is **two frames of the agent's own stdout**:
its answer to marion's `initialize` (id 0, the exact request `acp::initialize_request` builds) and
its answer to `session/new` (id 1, `{"cwd": <scratch>, "mcpServers": []}`). Nothing was
authenticated: no `authenticate` request was ever sent, no login command was run, and `HOME` was
the operator's own. An agent that needs an account is recorded at the wall, in its own words.

## Why this was measured

`acp::AGENTS` held five rows. The ACP registry (`agentclientprotocol/registry`, 2026-09-27) lists
~50 agents, and marion's generic path (`acp:<command>`) is meant to run any of them. This asks,
per agent: who it says it is (`agentInfo`), what it needs before a session opens (`authMethods`,
the `session/new` answer), and which session selects it advertises (`configOptions` / `modes`,
categories `model` and `mode`, which marion sets through `acp::SessionSelect`).

## What was found

"env" is what the probe set to reach `session/new`; every value was a dummy (`canned`) or a
loopback URL, never a real credential. Agents marked *provider* opened only once pointed at a
provider through their own env — on an operator's machine, their own configuration does that.

| file | argv | agentInfo | session/new | env |
|---|---|---|---|---|
| `opencode` | `opencode acp` | `OpenCode 1.18.32` | opened: model, mode | — |
| `kilo` | `kilo acp` | `Kilo 7.8.1` | opened: model, mode | — |
| `claude-acp` | `npx -y @agentclientprotocol/claude-agent-acp@0.66.0` | `@agentclientprotocol/claude-agent-acp 0.66.0` | opened: model, mode | — (operator's own claude login) |
| `codex-acp` | `codex-acp` | `@agentclientprotocol/codex-acp 1.13.1` | opened: model, mode | — (operator's own codex login) |
| `copilot` | `copilot --acp` | `Copilot 1.0.83` | opened: mode (no model select) | `COPILOT_AUTO_UPDATE=false` |
| `qwen` | `qwen --acp` | `qwen-code 0.23.0` | opened: model, mode — *provider* | `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_MODEL` |
| `goose` | `goose acp` | `goose 1.52.0` | opened: model, mode — *provider* | `GOOSE_PROVIDER=openai`, `GOOSE_MODEL`, `OPENAI_HOST`, `OPENAI_API_KEY` |
| `fast-agent` | `fast-agent-acp -x` | `fast-agent-acp 0.10.37` | opened: mode via `modes` only — *provider* | `FAST_AGENT_MODEL=generic.canned`, `GENERIC_BASE_URL`, `GENERIC_API_KEY` |
| `vibe` | `vibe-acp` | `@mistralai/mistral-vibe 2.25.8` | opened: model, mode — *provider* | `MISTRAL_API_KEY` |
| `vtcode` | `vtcode acp` | `vtcode 0.169.0` | opened: model (its agent select has no category) — *provider* | `VT_ACP_ENABLED=1` |
| `docker-agent` | `docker-agent serve acp <agent.yaml>` | `docker agent v1.144.0` | opened: model, mode — *provider* | `OPENAI_API_KEY`; the agent file names a loopback `base_url` |
| `gemini` | `gemini --acp` | `gemini-cli 0.53.0` | **refused** `-32000` (Gemini Code Assist ineligibility, as S20) | — |
| `auggie` | `auggie --acp` | `auggie 0.36.0` | **refused** `-32000` "run `auggie login`" | — |
| `qoder` | `qodercli --acp` | `qoder-cli 1.1.64` | **refused** `-32000` "Authentication required" | — |
| `droid` | `droid exec --output-format acp-daemon` | `@factory/cli 0.228.0` | **refused** `-32000`, and it **starts a device pairing on `session/new`** (code redacted to `<CODE>`) | — |
| `cline` | `cline --acp` (the pinned 3.0.61) | `cline 3.0.61` | **refused** `-32000` "Call authenticate before starting a session" | — |
| `pi-acp` | `pi-acp` | `pi-acp 0.0.34` | **refused** `-32603`: the shim calls `get_available_thinking_levels`, which the installed `pi` 0.80.2 does not have (version skew, not auth) | — |

Beyond these two frames (recorded in the probe's notes, not in these files):

- **Bridge start from `session/new`.** With a stdio `marion` server declared in `mcpServers`,
  `opencode`, `kilo`, `claude-agent-acp` (0.81.2, the registry's current), `codex-acp`, `qwen`, `vibe`, `fast-agent` and
  `docker-agent` started it (`initialize`, `tools/list`) before the first prompt. `copilot` 1.0.83
  did not (S28's quirk, reconfirmed: the bridge reaches it only through `--additional-mcp-config`),
  nor did `vtcode` or `goose` within 8 s of `session/new`.
- **Turns against a local endpoint, $0.00.** `qwen`, `goose`, `fast-agent` and `docker-agent` ran a
  full `session/prompt` to `end_turn` against marion's canned provider (`marion-canned`) and a
  loopback OpenAI-compatible fake. `fast-agent` presented the bridge's tool to the model as
  `marion__report`, asked `session/request_permission` (allow_once / allow_always / reject_once /
  reject_always), called it for real (`tools/call` reached the server) and titled the ACP
  `tool_call` **`marion/report`** — which `acp::Reading::Generic` already reads. `docker-agent`
  called it too, but under an **opaque hashed name** (`acp_<hash>`, both to the model and in the
  `tool_call` title), so no reading can recover marion's verb from its transcript. `qwen` 0.23.0
  started the server but did not offer its tools in the first model request (it defers MCP tools
  behind its own `tool_search`).
- **Not installable here without an account or a pipe-to-shell installer:** Kiro, Cursor agent,
  Junie, Kimi Code (the `kimi-cli` PyPI package is the archived predecessor; its `kimi acp` prints
  "no longer maintained" and exits), OpenHands, Code Assistant (source build only), Amp (the
  `amp-acp` shim wraps the account-only `amp`), Blackbox. Crush's ACP server is an unmerged PR;
  Warp hosts ACP agents and exposes none.

## Redaction

Trimmed by a one-off script from the full transcripts, neither of which was committed:

- only frames 0 and 1; every `session/update` notification (skill and command catalogues) dropped;
- every `model` select's roster (the operator's providers and account tier) replaced by one
  `<MODEL>` option, and goose's and vtcode's `provider` select by `<PROVIDER>`; the unstable
  `models` object dropped for the same reason;
- `sessionId` → `<UUID>`; scratch `cwd` paths → `<SCRATCH>`; `_meta`, descriptions and
  `authMethods[].args/env` dropped;
- droid's device-pairing code → `<CODE>`. No pairing was completed and none can be from this file.

Host, username and credential scan: clean (no `$HOME`, no user name, no key material).

## Which tests read it

- `crates/marion-harness/src/acp.rs` — `the_mcp_transports_an_agent_advertises_are_read_off_its_handshake`,
  and `every_refinement_row_matches_its_s33_capture`, the sweep that holds each `acp::AGENTS` row's
  `agent_info` and `reach` (opened or refused; the `model` and `mode` selects) to its file here.
  Two files are evidence only and have no row: `docker-agent.jsonl` (its launch needs the
  operator's own agent file) and `droid.jsonl` (its `session/new` starts a device pairing, and the
  doctor's `--adapter` mode opens a session on every row).
