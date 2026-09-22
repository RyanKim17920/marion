# S31 — what each harness does with a second turn (turn delivery, P0)

Measured 2026-09-22, macOS darwin 25.5.0. **$0.00 metered**: every claude, opencode, codex, copilot
and qwen run in `p0b/` and every claude run in `p0a/` went to a loopback provider (a scripted
Anthropic/OpenAI SSE server that logs each request before it answers or holds, or marion's
`canned` behind a delay proxy). The channel probe (`p0a/ch2`) used the operator's claude.ai login,
sent only to loopback. codex-acp and copilot `--acp` made a few real calls on subscription logins.
The drivers and full transcripts stayed in the session scratchpad; what is here is trimmed to the
summaries and the short transcripts each fact rests on. Paths are scrubbed to `<scratch>`/`~`.

Versions: claude 2.1.280 (1a/1b/ch2 repeated on 2.1.276 with identical results; **2.1.269, the
admitted pin, was not installed and none of this was measured on it**), opencode 1.18.32,
claude-agent-acp 0.81.0, codex-acp 1.13.0, copilot 1.0.83/1.0.87, codex-cli 0.147.0 (the pinned
shim), qwen 0.23.0, goose 1.51.0.

## Why this was measured

marion's turn delivery (`marion_harness::spec::TurnDelivery`) needs, per row and per shape, the
one way a second message reaches a running node — and what the harness does with a message written
while a turn is still in flight. None of it was on record.

## p0a — typed turns written mid-turn

| file | fact |
|---|---|
| `p0a/b3.summary.json` | claude `-p` stream-json: a user frame written while a two-tool turn is held at the provider is **folded** into the running turn at the next tool-result boundary, as a trailing `role:"system"` "The user sent a new message while you were working" entry. **One `result`** for the two frames. |
| `p0a/b.summary.json`, `p0a/b2.summary.json` | the same frame, when the held request is the turn's last, is **queued** as the next turn and gets its own `result`. Never dropped. |
| `p0a/ch2.summary.json` | claude interactive `notifications/claude/channel`: the same fold rule ("A message arrived from … while you were working"). Channels are **refused under API-key/token auth** ("Channels are not currently available"); they only worked with a claude.ai login. |
| `p0a/acp-opencode-fold.summary.json`, `p0a/acp-claude-acp-fold.summary.json` | ACP `session/prompt` while one is in flight: accepted and folded/queued into the running loop, and **both responses arrive only when the loop drains** (P3's completion is not separately observable). |
| `p0a/acp-codex-acp.summary.json` | codex-acp steers the second prompt into the turn and **never answers the first** (id 5 still pending after 120 s). |
| `p0a/acp-copilot.summary.json` | copilot `--acp` **supersedes** the first: it returns `end_turn` with no output and the previous prompt's usage byte for byte. |

So on every typed surface a mid-turn write either breaks one-result-per-turn pairing (fold) or
loses a response (codex-acp, copilot). marion therefore queues on its own side and delivers a typed
turn only at a turn boundary.

## p0b — headless resume, and TUI paste injection

| file | fact |
|---|---|
| `p0b/codex/run2.jsonl` | `codex exec -C <cwd> resume <thread> --json … -c mcp_servers.marion.* …` continues the thread: the same `thread.started` id as run1. |
| `p0b/codex/run4.jsonl`, `p0b/codex/mcp-methods.txt` | the same resume **without** the `-c mcp_servers.marion.*` pairs continues the thread but never spawns the MCP server: `mcp-methods.txt` shows three `initialize`s, for run1, run2 (with `-c`) and run3, and none for run4. The redeclaration must ride every resume. |
| `p0b/opencode/mem2.err` | `opencode run --session <id>` under `OPENCODE_DB=:memory:` (marion's row until this fixture) exits 1: `Error: Session not found`. The in-memory DB died with the first process. |
| `p0b/opencode/db1.jsonl`, `p0b/opencode/db2.jsonl` | with `OPENCODE_DB=<agent dir>/node.db` the second life carries the same `sessionID` and the first life's history. |
| `p0b/copilot/run2.jsonl` | copilot `-p … --resume=<sessionId>` continues the session (same `sessionId`). |
| `p0b/qwen/run2.jsonl` | qwen `--resume <session_id> -p …` continues the session. |
| `p0b/tui/<h>.result.json` | codex, opencode, copilot and claude TUIs all enable DECSET 2004 at boot; `\e[200~text\e[201~` then `\r` **submits at 0 ms** (also 50/150/300/600). Idle output is ~0 B/s; busy spinners repaint at ≤ 454 ms gaps (codex 114, opencode 361, copilot 336, claude 454), so **1500 ms of output quiet is a reliable idle signal**. Enter while busy is never dropped and never interrupts. |
| `p0b/tui/<h>-pastereps.result.json` | 15 further paste trials each (short, 3-line, 3 KB; 0 and 50 ms): every one submitted on the first CR, byte-exact. Unbracketed text + `\r` at 0 ms **fails on codex** (its paste-burst heuristic turns the CR into a newline), so a paste is always bracketed. |

goose 1.51.0 also continues a session by a caller-chosen name (`run --resume -n <name>`), which
needs marion's `--no-session` dropped; its trace is too large to commit and the row stays `None`.
