# Harness conformance matrix

Written by `crates/marion-supervisor/tests/conformance` (`scripts/conformance.sh`). Every run is $0: marion's canned provider on loopback, scratch homes, no login. `n/a` is UNSUPPORTED, with the reason (from the row) below. Per-probe transcripts are in each harness's directory.

| harness | version | P-version | P-launch | P-tools | P-activity | P-approval | P-midturn | P-interrupt | P-resume | P-lifecycle | P-errors | P-tui |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| acp:claude-acp | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:codex-acp | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:copilot | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:gemini | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:opencode | OpenCode 1.18.32 | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | **FAIL** | n/a |
| agy | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| claude-code | 2.1.283 | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | **FAIL** | **FAIL** |
| codex | 0.155.1 | PASS | PASS | **FAIL** | PASS | PASS | n/a | PASS | PASS | PASS | **FAIL** | **FAIL** |
| copilot | 1.0.83 | PASS | PASS | PASS | PASS | PASS | n/a | PASS | PASS | **FAIL** | **FAIL** | n/a |
| gemini | 0.53.0 | PASS | PASS | PASS | PASS | PASS | n/a | PASS | n/a | **FAIL** | PASS | n/a |
| goose | 1.52.0 | PASS | PASS | PASS | PASS | **FAIL** | n/a | PASS | n/a | PASS | PASS | n/a |
| opencode | 1.18.32 | PASS | PASS | PASS | PASS | PASS | n/a | PASS | PASS | PASS | **FAIL** | n/a |
| qwen | 0.23.0 | PASS | PASS | PASS | PASS | PASS | n/a | PASS | PASS | **FAIL** | **FAIL** | n/a |

## Findings per cell

### acp:claude-acp unknown

- **P-version** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-launch** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-tools** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-activity** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-approval** UNSUPPORTED: P-activity did not run, so the granted case is unknown
- **P-midturn** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-interrupt** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-resume** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-lifecycle** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-errors** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### acp:codex-acp unknown

- **P-version** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-launch** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-tools** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-activity** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-approval** UNSUPPORTED: P-activity did not run, so the granted case is unknown
- **P-midturn** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-interrupt** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-resume** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-lifecycle** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-errors** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### acp:copilot unknown

- **P-version** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-launch** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-tools** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-activity** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-approval** UNSUPPORTED: P-activity did not run, so the granted case is unknown
- **P-midturn** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-interrupt** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-resume** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-lifecycle** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-errors** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### acp:gemini unknown

- **P-version** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-launch** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-tools** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-activity** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-approval** UNSUPPORTED: P-activity did not run, so the granted case is unknown
- **P-midturn** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-interrupt** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-resume** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-lifecycle** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-errors** UNSUPPORTED: marion will not compile a canned launch: compile: acp: ACP names no provider, base URL or credential at any point in its handshake, and marion has never measured a way to point this particular agent at one. Run it without --canned, on your own login, or pick an agent whose canned recipe is measured
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### acp:opencode OpenCode 1.18.32

- **P-version** PASS: OpenCode 1.18.32 (admitted in PINNED_HARNESSES); row states no switch (no single program: the switch is the bound agent's; only the opencode agent's canned recipe carries one (opencode's row))
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; still running; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `marion_report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 8.0 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 311, output: 24, cache_read: 15, cache_write: 0 } (provider sent [(111, 10, 5), (211, 17, 10), (311, 24, 15)]); session ses_f1b4c5bc8ffemJkn0W0wnPTvUz ; activity none (row: no activity rule)
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Answered), 0 permission ask(s) reached marion
- **P-midturn** PASS: queued as its own turn (2 turn ends; requests [4]); 2 prompt answer(s)
- **P-interrupt** PASS: the cancel ended the turn in 0.2 s (still running); 1 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session ses_f1b4c000cffewMS6VHxBMuWzMM -> ses_f1b4c000cffewMS6VHxBMuWzMM; resume refusal: none
- **P-lifecycle** PASS: idle stdin EOF: exit 0, left []; SIGTERM mid-turn: signal 15, left []
- **P-errors** FAIL: 401: ended after 2.2 s, 2 request(s), still running; auth line None; stream failure Some("Internal error: Incorrect API key provided: dummy."); prompt answer {"code":-32603,"data":{"errorName":"APIError","service":"session"},"message":"Internal error: Incorrect API key provided: dummy."} | 429: still running after 46.2 s, 8 request(s), still running; auth line None; stream failure None | 500: still running after 49.1 s, 8 request(s), still running; auth line None; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### agy unknown

- **P-version** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned provider route: it runs only on the operator's own login, so launch it live (marion run --live)
- **P-launch** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned provider route: it runs only on the operator's own login, so launch it live (marion run --live)
- **P-tools** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned provider route: it runs only on the operator's own login, so launch it live (marion run --live)
- **P-activity** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned provider route: it runs only on the operator's own login, so launch it live (marion run --live)
- **P-approval** UNSUPPORTED: P-activity did not run, so the granted case is unknown
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): s32 on 1.2.8: `--conversation <id> -p …` continues the conversation under the same id and remembers the first turn
- **P-interrupt** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned provider route: it runs only on the operator's own login, so launch it live (marion run --live)
- **P-resume** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned provider route: it runs only on the operator's own login, so launch it live (marion run --live)
- **P-lifecycle** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned provider route: it runs only on the operator's own login, so launch it live (marion run --live)
- **P-errors** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned provider route: it runs only on the operator's own login, so launch it live (marion run --live)
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### claude-code 2.1.283

- **P-version** PASS: 2.1.283 (admitted in PINNED_HARNESSES); no-self-update env DISABLE_AUTOUPDATER=1 carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"anthropic"}; still running; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `mcp__marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 4.4 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 322, output: 27, cache_read: 15, cache_write: 0 } (provider sent [(111, 10, 5), (211, 17, 10)]); session f3e0520a-56c4-449d-942b-a6379d3007a1 ; activity mcp__marion__report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("marion denies `mcp__marion__report`: no operator answers a child's ask")), 1 permission ask(s) reached marion
- **P-midturn** PASS: folded into the running turn (1 turn end; requests [4])
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (still running); 1 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session daa37586-7d2f-49e8-8a52-18e69f0101cb -> daa37586-7d2f-49e8-8a52-18e69f0101cb; resume refusal: none
- **P-lifecycle** PASS: idle stdin EOF: exit 0, left []; SIGTERM mid-turn: exit 143, left []
- **P-errors** FAIL: 401: still running after 46.2 s, 7 request(s), still running; auth line None; stream failure None | 429: still running after 46.9 s, 7 request(s), still running; auth line None; stream failure None | 500: still running after 45.3 s, 7 request(s), still running; auth line None; stream failure None
- **P-tui** FAIL: DECSET 2004 at boot: true; first screen [" project, or work from your team). If not, take a moment to review what's in this folder first.", " Claude Code'll be able to read, edit, and execute files here.", " Security guide", " ❯ No, exit", "   Yes, I trust this folder", " Enter to confirm · Esc to cancel"]; bracketed paste + CR submitted: NO; output quiet 1500 ms after the turn: false; `/mcp` screen names marion: false

### codex 0.155.1

- **P-version** PASS: 0.155.1 (admitted in PINNED_HARNESSES); no-self-update pair check_for_update_on_startup=false carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"responses"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** FAIL: first request lists `mcp__marion__report`: false; with the bridge 4 s slow and no gate of marion's, the first request came 1.2 s after the spawn and did NOT list marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 322, output: 27, cache_read: 15, cache_write: 0 } (provider sent [(111, 10, 5), (211, 17, 10)]); session 01a0e4a9-cb57-76d2-9996-f5b18dc6aec0 ; activity report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("failed")), 0 permission ask(s) reached marion
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): S31 p0b/codex (0.147.0): `exec -C <cwd> resume <id>` continues the thread; the -c mcp_servers.marion.* redeclaration must ride every resume, and stdin is read once before the first request, never mid-run
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (exit 1); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session 01a0e4a9-d259-7440-ab7c-995dea6eedd0 -> 01a0e4a9-d259-7440-ab7c-995dea6eedd0; resume refusal: none
- **P-lifecycle** PASS: turn end: exit 0, left []; SIGTERM mid-turn: signal 15, left []
- **P-errors** FAIL: 401: ended after 6.6 s, 6 request(s), exit 1; auth line None; stream failure None | 429: ended after 0.2 s, 1 request(s), exit 1; auth line None; stream failure None | 500: ended after 25.7 s, 30 request(s), exit 1; auth line None; stream failure None
- **P-tui** FAIL: DECSET 2004 at boot: true; first screen ["> You are in <SCRATCH>/p-tui/repo", "  Do you trust the contents of this directory? Working with untrusted contents comes with higher risk of prompt", "  injection. Trusting the directory allows project-local config, hooks, and exec policies to load.", "› 1. Yes, continue", "  2. No, quit", "  Press enter to continue"]; bracketed paste + CR submitted: only on a second paste — the first-paint screen (above) swallowed the first; output quiet 1500 ms after the turn: true; `/mcp` screen names marion: true

### copilot 1.0.83

- **P-version** PASS: 1.0.83 (admitted in PINNED_HARNESSES); no-self-update env COPILOT_AUTO_UPDATE=false carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `marion-report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 4.8 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage none (row: no usage rule) (provider sent [(111, 10, 5), (211, 17, 10)]); session 3a4a4b69-f2dc-4c42-b0b7-d2370d34c573 ; activity marion-report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("Permission denied and could not request permission from user")), 0 permission ask(s) reached marion
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): S31 p0b/copilot (1.0.83): `-p … --resume=<sessionId>` continues the session
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (signal 2); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session 134f9874-cf86-4483-9f4e-46ca70adcc99 -> 134f9874-cf86-4483-9f4e-46ca70adcc99; resume refusal: none
- **P-lifecycle** FAIL: turn end: exit 0, left []; SIGTERM mid-turn: signal 15, left [(59154, "/opt/homebrew/lib/node_modules/@github/copilot/node_modules/@github/copilot-darwin-arm64/copilot -p CONFLIFETERM: follow the conformance script.\\012\\012When you have finished, call the `report` tool of the marion MCP server exactly once, with a one-sentence `narrative` of what you did. Your final message is not returned to whoever delegated this task; only that report is. --output-format json -C <SCRATCH>/copilot/p-lifecycle-sigterm/repo --disable-builtin-mcps --no-custom-instructions --model marion-canned --available-tools=marion-report,view,create,edit,apply_patch --allow-tool=marion(report) --allow-tool=write --additional-mcp-config @<SCRATCH>/copilot/p-lifecycle-sigterm/node/mcp.json"), (59463, "<WORKTREE>/target/debug/marion-supervisor mcp")]
- **P-errors** FAIL: 401: ended after 2.6 s, 3 request(s), exit 1; auth line None; stream failure Some("Authentication failed with provider at http://127.0.0.1:54881/v1 (HTTP 401).\n  Check your COPILOT_PROVIDER_API_KEY or COPILOT_PROVIDER_BEARER_TOKEN.") | 429: still running after 45.0 s, 5 request(s), still running; auth line None; stream failure None | 500: ended after 35.5 s, 6 request(s), exit 1; auth line None; stream failure Some("Failed to get response from the AI model; retried 5 times (total retry wait time: 34.75 seconds) Last error: 500 internal server error")
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### gemini 0.53.0

- **P-version** PASS: 0.53.0 (admitted in PINNED_HARNESSES); no-self-update document keys carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"gemini"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `mcp_marion_report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 5.0 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 322, output: 27, cache_read: 15, cache_write: 0 } (provider sent [(111, 10, 5), (211, 17, 10)]); session ceefc1b4-1864-424c-bc9f-3abbde9021cc ; activity mcp_marion_report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("error: Tool \"mcp_marion_report\" not found. Did you mean one of: \"update_topic\", \"list_directory\", \"grep_search\"?")), 0 permission ask(s) reached marion
- **P-midturn** UNSUPPORTED: headless delivery None { note: "gemini's --resume takes `latest` or an index, not a session id, and S31 did not probe it; gemini 0.53 --acp refuses session/new (S31 p0a)" }
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (exit 0); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** UNSUPPORTED: the row states no resume spelling (`resume: None`)
- **P-lifecycle** FAIL: turn end: exit 0, left []; SIGTERM mid-turn: still running, left [(12341, "node /opt/homebrew/bin/gemini -m gemini-2.5-flash --output-format stream-json --approval-mode auto_edit -p CONFLIFETERM: follow the conformance script.\\012\\012When you have finished, call the `report` tool of the marion MCP server exactly once, with a one-sentence `narrative` of what you did. Your final message is not returned to whoever delegated this task; only that report is."), (12893, "/opt/homebrew/Cellar/node/26.10.0_1/bin/node --max-old-space-size=12288 /opt/homebrew/bin/gemini -m gemini-2.5-flash --output-format stream-json --approval-mode auto_edit -p CONFLIFETERM: follow the conformance script.\\012\\012When you have finished, call the `report` tool of the marion MCP server exactly once, with a one-sentence `narrative` of what you did. Your final message is not returned to whoever delegated this task; only that report is."), (16061, "<WORKTREE>/target/debug/marion-supervisor mcp")]
- **P-errors** PASS: 401: ended after 2.9 s, 1 request(s), exit 145; auth line Some("Error when talking to Gemini API Full report available at: <TMP>/gemini-client-error-Turn.run-sendMessageStream-2026-09-27T21-00-57-324Z.json _ApiError: {\"error\":{\"code\":401,\"message\":\"API key not valid. Please pass a valid API key.\",\"status\":\"UNAUTHENTICATED\"}}"); stream failure Some("gemini result status: error") | 429: still running after 45.0 s, 4 request(s), still running; auth line None; stream failure None | 500: still running after 45.0 s, 4 request(s), still running; auth line None; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### goose 1.52.0

- **P-version** PASS: 1.52.0 (admitted in PINNED_HARNESSES); row states no switch (1.49.0 never updates itself on `run`/`session`: no update check in the binary's strings, no `GOOSE_*` update variable; `goose update` is explicit only)
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 4.4 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 547, output: 41, cache_read: 25, cache_write: 0 } (provider sent [(111, 10, 5), (211, 17, 10), (311, 24, 15)]); session none (row: no session rule) ; activity marion__report
- **P-approval** FAIL: granted: `report` answered true; ungranted: `report` Some(Answered), 0 permission ask(s) reached marion — the grant is NOT load-bearing: the harness ran marion's tool without it
- **P-midturn** UNSUPPORTED: headless delivery None { note: "S31 p0b/goose (1.51.0): `run --resume -n <name>` continues a session by a name the caller chooses, which needs this row's --no-session dropped; until it is, there is no second turn" }
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (exit 0); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** UNSUPPORTED: the row states no resume spelling (`resume: None`)
- **P-lifecycle** PASS: turn end: exit 0, left []; SIGTERM mid-turn: signal 15, left []
- **P-errors** PASS: 401: ended after 0.1 s, 2 request(s), exit 1; auth line Some("Error: Ran into this error: Authentication error: Authentication failed for http://127.0.0.1:55303/v1/chat/completions. Status: 401 Unauthorized. Response: Incorrect API key provided: dummy.."); stream failure None | 429: ended after 6.7 s, 7 request(s), exit 0; auth line None; stream failure None | 500: ended after 6.3 s, 7 request(s), exit 0; auth line None; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### opencode 1.18.32

- **P-version** PASS: 1.18.32 (admitted in PINNED_HARNESSES); no-self-update env OPENCODE_DISABLE_AUTOUPDATE=1 carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `marion_report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 6.2 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 322, output: 27, cache_read: 15, cache_write: 0 } (provider sent [(111, 10, 5), (211, 17, 10)]); session ses_f1b52a8b2ffeNNQUoF3T9Q56J3 ; activity marion_report
- **P-approval** PASS: `report` answered with no grant: true (S13 on 1.17.3: with no `permission` entry for marion's tool the call runs; `ask` auto-rejects at exit 0, so marion states no permission at all)
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): S31 p0b/opencode (1.18.32): `run --session <id>` continues the session with the store in a file OPENCODE_DB (db1/db2); `:memory:` failed with Session not found
- **P-interrupt** PASS: the cancel did not end the turn within 10 s; 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session ses_f1b526e22ffeLT2hsKXrn9ZpMy -> ses_f1b526e22ffeLT2hsKXrn9ZpMy; resume refusal: none
- **P-lifecycle** PASS: turn end: exit 0, left []; SIGTERM mid-turn: signal 15, left []
- **P-errors** FAIL: 401: ended after 2.3 s, 1 request(s), exit 1; auth line None; stream failure Some("Incorrect API key provided: dummy.") | 429: still running after 45.0 s, 5 request(s), still running; auth line None; stream failure None | 500: still running after 45.0 s, 5 request(s), still running; auth line None; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### qwen 0.23.0

- **P-version** PASS: 0.23.0 (admitted in PINNED_HARNESSES); no-self-update env QWEN_CODE_SKIP_UPDATE_CHECK_ONCE=true carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `mcp__marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 7.7 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 337, output: 27, cache_read: 15, cache_write: 0 } (provider sent [(111, 10, 5), (211, 17, 10)]); session 40f58e4d-2dfc-4e43-b0d8-b1f5c2077e93 ; activity mcp__marion__report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("Qwen Code requires permission to use \"mcp__marion__report\", but that permission was declined (non-interactive mode cannot prompt for confirmation).")), 0 permission ask(s) reached marion
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): S31 p0b/qwen (0.23.0): `--resume <session_id> -p …` continues the session
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (signal 2); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session 9d3b7205-113b-4d1e-8459-98e24206d3ae -> 9d3b7205-113b-4d1e-8459-98e24206d3ae; resume refusal: none
- **P-lifecycle** FAIL: turn end: exit 0, left []; SIGTERM mid-turn: signal 15, left [(84776, "/opt/homebrew/Cellar/node/26.10.0_1/bin/node --expose-gc /opt/homebrew/lib/node_modules/@qwen-code/qwen-code/cli.js --yolo --core-tools mcp__marion__report read_file write_file --exclude-tools agent enter_worktree exit_worktree get_goal list_agents record_artifact report_findings send_message skill task_stop tool_search update_goal -p CONFLIFETERM: follow the conformance script.\\012\\012When you have finished, call the `report` tool of the marion MCP server exactly once, with a one-sentence `narrative` of what you did. Your final message is not returned to whoever delegated this task; only that report is. --output-format stream-json"), (85384, "/opt/homebrew/Cellar/node/26.10.0_1/bin/node --expose-gc /opt/homebrew/lib/node_modules/@qwen-code/qwen-code/cli.js --yolo --core-tools mcp__marion__report read_file write_file --exclude-tools agent enter_worktree exit_worktree get_goal list_agents record_artifact report_findings send_message skill task_stop tool_search update_goal -p CONFLIFETERM: follow the conformance script.\\012\\012When you have finished, call the `report` tool of the marion MCP server exactly once, with a one-sentence `narrative` of what you did. Your final message is not returned to whoever delegated this task; only that report is. --output-format stream-json"), (85575, "<WORKTREE>/target/debug/marion-supervisor mcp"), (86227, "caffeinate -is")]
- **P-errors** FAIL: 401: ended after 1.6 s, 1 request(s), exit 0; auth line None; stream failure None | 429: still running after 45.0 s, 20 request(s), still running; auth line None; stream failure None | 500: still running after 45.0 s, 20 request(s), still running; auth line None; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)
