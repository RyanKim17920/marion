# Harness conformance matrix

Written by `crates/marion-supervisor/tests/conformance` (`scripts/conformance.sh`). Every run is $0: marion's canned provider on loopback, scratch homes, no login. `n/a` is UNSUPPORTED, with the reason (from the row) below. Per-probe transcripts are in each harness's directory.

| harness | version | P-version | P-launch | P-tools | P-activity | P-approval | P-midturn | P-interrupt | P-resume | P-lifecycle | P-errors | P-tui |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| acp:claude-acp | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:cline | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:codex-acp | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:copilot | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:goose | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| acp:opencode | OpenCode 1.18.33 | PASS | PASS | PASS | PASS | n/a | PASS | PASS | PASS | PASS | **FAIL** | n/a |
| acp:qwen | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| agy | unknown | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| claude-code | 2.1.301 | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | **FAIL** |
| cline | 3.0.66 | PASS | PASS | **FAIL** | PASS | PASS | n/a | PASS | n/a | **FAIL** | **FAIL** | n/a |
| codex | 0.159.2 | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS | PASS |
| copilot | 1.0.89 | PASS | PASS | PASS | PASS | PASS | n/a | PASS | PASS | **FAIL** | PASS | n/a |
| goose | 1.55.0 | PASS | PASS | PASS | PASS | n/a | n/a | PASS | n/a | PASS | PASS | n/a |
| opencode | 1.18.33 | PASS | PASS | PASS | PASS | PASS | n/a | PASS | PASS | PASS | **FAIL** | n/a |
| pi | 0.99.1 | PASS | PASS | PASS | PASS | n/a | PASS | PASS | PASS | PASS | PASS | n/a |
| qwen | 0.24.7 | PASS | PASS | PASS | PASS | PASS | n/a | PASS | PASS | PASS | **FAIL** | n/a |

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

### acp:cline unknown

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

### acp:goose unknown

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

### acp:opencode OpenCode 1.18.33

- **P-version** PASS: OpenCode 1.18.33 (admitted in PINNED_HARNESSES); row states no switch (no single program: the switch is the bound agent's; only the opencode agent's canned recipe carries one (opencode's row))
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; still running; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `marion_report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 6.5 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 90, output: 20, cache_read: 10, cache_write: 0, reasoning: Some(5) } (provider sent [(111, 10, 5), (211, 17, 10), (311, 24, 15)]); session ses_f0bb0732bffenYE4H1e8o2LSN4 ; activity none (row: no activity rule)
- **P-approval** UNSUPPORTED: granted: `report` answered true; ungranted: `report` answered with no permission ask reaching marion, so marion's answer to an ask was not exercised
- **P-midturn** PASS: queued as its own turn (2 turn ends; requests [4]); 2 prompt answer(s)
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (still running); 1 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session ses_f0bb04ad4ffeGuEfFYv8ruFFRM -> ses_f0bb04ad4ffeGuEfFYv8ruFFRM; resume refusal: none
- **P-lifecycle** PASS: idle stdin EOF: exit 0, left []; SIGTERM mid-turn: signal 15, left []
- **P-errors** FAIL: 401: ended after 1.9 s, 2 request(s), still running; cause Auth { line: "Internal error: Incorrect API key provided: dummy." }; stream failure Some("Internal error: Incorrect API key provided: dummy."); no frame reads as a refused credential; prompt answer {"code":-32603,"data":{"errorName":"APIError","service":"session"},"message":"Internal error: Incorrect API key provided: dummy."} | 429: still running after 46.0 s, 8 request(s), still running; cause none said; stream failure None | 500: still running after 46.4 s, 8 request(s), still running; cause none said; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### acp:qwen unknown

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

### agy unknown

- **P-version** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned or endpoint provider route: it runs only on the operator's own login, so launch it without --canned or a provider
- **P-launch** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned or endpoint provider route: it runs only on the operator's own login, so launch it without --canned or a provider
- **P-tools** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned or endpoint provider route: it runs only on the operator's own login, so launch it without --canned or a provider
- **P-activity** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned or endpoint provider route: it runs only on the operator's own login, so launch it without --canned or a provider
- **P-approval** UNSUPPORTED: P-activity did not run, so the granted case is unknown
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): s32 on 1.2.8: `--conversation <id> -p …` continues the conversation under the same id and remembers the first turn
- **P-interrupt** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned or endpoint provider route: it runs only on the operator's own login, so launch it without --canned or a provider
- **P-resume** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned or endpoint provider route: it runs only on the operator's own login, so launch it without --canned or a provider
- **P-lifecycle** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned or endpoint provider route: it runs only on the operator's own login, so launch it without --canned or a provider
- **P-errors** UNSUPPORTED: marion will not compile a canned launch: compile: agy: agy has no canned or endpoint provider route: it runs only on the operator's own login, so launch it without --canned or a provider
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### claude-code 2.1.301

- **P-version** PASS: 2.1.301 (admitted in PINNED_HARNESSES); no-self-update env DISABLE_AUTOUPDATER=1 carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"anthropic"}; still running; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `mcp__marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 4.6 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 322, output: 27, cache_read: 15, cache_write: 0, reasoning: None } (provider sent [(111, 10, 5), (211, 17, 10)]); session a6be17bb-3f52-4410-be5e-e6cbd970a832 ; activity mcp__marion__report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("marion denies `mcp__marion__report`: no operator answers a child's ask")), 1 permission ask(s) reached marion
- **P-midturn** PASS: folded into the running turn (1 turn end; requests [4])
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (still running); 1 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session fbd21be0-80f6-4f65-a5f4-ac40ae21fc48 -> fbd21be0-80f6-4f65-a5f4-ac40ae21fc48; resume refusal: none
- **P-lifecycle** PASS: idle stdin EOF: exit 0, left []; SIGTERM mid-turn: exit 143, left []
- **P-errors** PASS: 401: still running after 45.4 s, 7 request(s), still running; cause Auth { line: "HTTP 401 authentication_failed" }; stream failure None; refused credential read off a frame: "HTTP 401 authentication_failed" | 429: still running after 45.4 s, 7 request(s), still running; cause RateLimit { line: "HTTP 429 rate_limit" }; stream failure None | 500: still running after 45.4 s, 7 request(s), still running; cause Outage { line: "HTTP 500 server_error" }; stream failure None
- **P-tui** FAIL: DECSET 2004 at boot: true; first screen ["  1  function greet() {", "  2 -  console.log(\"Hello, World!\");", "  2 +  console.log(\"Hello, Claude!\");", "  3  }", " ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌", "  Syntax theme: Monokai Extended (ctrl+t to disable)"]; boot dialog none of the row's on the first screen; bracketed paste + CR submitted: NO; output quiet 1500 ms after the turn: false; `/mcp` screen names marion: false

### cline 3.0.66

- **P-version** PASS: 3.0.66 (admitted in PINNED_HARNESSES); no-self-update env CLINE_NO_AUTO_UPDATE=1 carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** FAIL: first request lists `marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 10.0 s after the spawn and did NOT list marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 200, output: 30, cache_read: 20, cache_write: 0, reasoning: None } (provider sent [(111, 10, 5), (211, 17, 10)]); session none (row: no session rule) ; activity marion__report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` None, 0 permission ask(s) reached marion
- **P-midturn** UNSUPPORTED: headless delivery None { note: "S27: cline 3.0.61's `--id <id>` forces interactive mode and exits 1 headless, so there is no headless second turn; not re-measured in S31" }
- **P-interrupt** PASS: the cancel ended the turn in 0.1 s (exit 0); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** UNSUPPORTED: the row states no resume spelling (`resume: None`)
- **P-lifecycle** FAIL: turn end: exit 0, left []; SIGTERM mid-turn: still running, left [(43210, "cline --json -c <SCRATCH>/cline/p-lifecycle-sigterm/repo --config <SCRATCH>/cline/p-lifecycle-sigterm/node/cfg --data-dir <SCRATCH>/cline/p-lifecycle-sigterm/node/data --auto-approve true -m marion-canned CONFLIFETERM: follow the conformance script.\\012\\012When you have finished, call the `report` tool of the marion MCP server exactly once, with a one-sentence `narrative` of what you did. Your final message is not returned to whoever delegated this task; only that report is."), (43452, "<WORKTREE>/target/debug/marion-supervisor mcp")]
- **P-errors** FAIL: 401: ended after 1.4 s, 1 request(s), exit 1; cause Auth { line: "{\"ts\":\"2026-09-30T22:07:31.456Z\",\"type\":\"error\",\"message\":\"Incorrect API key provided: dummy.\"}" }; stream failure Some("Incorrect API key provided: dummy."); no frame reads as a refused credential | 429: still running after 45.0 s, 5 request(s), still running; cause none said; stream failure None | 500: still running after 45.0 s, 5 request(s), still running; cause none said; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### codex 0.159.2

- **P-version** PASS: 0.159.2 (admitted in PINNED_HARNESSES); no-self-update pair check_for_update_on_startup=false carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"responses"}; still running; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `mcp__marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 1.2 s after the spawn and did NOT list marion's tools (marion gates the first prompt on marion's server being ready)
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 322, output: 27, cache_read: 15, cache_write: 0, reasoning: Some(0) } (provider sent [(111, 10, 5), (211, 17, 10)]); session 01a0f44b-9c9d-7942-ad85-d6c4059a9df9 ; activity report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("failed: user rejected MCP tool call")), 0 permission ask(s) reached marion
- **P-midturn** PASS: folded into the running turn (1 turn end; requests [3])
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (still running); 2 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session 01a0f44b-e1f9-7203-a860-c8f45c44308b -> 01a0f44b-e1f9-7203-a860-c8f45c44308b; resume refusal: none
- **P-lifecycle** PASS: idle stdin EOF: exit 0, left []; SIGTERM mid-turn: exit 0, left []
- **P-errors** PASS: 401: ended after 7.0 s, 6 request(s), still running; cause Auth { line: "unexpected status 401 Unauthorized: Incorrect API key provided: dummy., url: http://127.0.0.1:55430/v1/responses" }; stream failure Some("unexpected status 401 Unauthorized: Incorrect API key provided: dummy., url: http://127.0.0.1:55430/v1/responses"); refused credential read off a frame: "unexpected status 401 Unauthorized: Incorrect API key provided: dummy., url: http://127.0.0.1:55430/v1/responses" | 429: ended after 0.3 s, 1 request(s), still running; cause RateLimit { line: "exceeded retry limit, last status: 429 Too Many Requests" }; stream failure Some("exceeded retry limit, last status: 429 Too Many Requests") | 500: ended after 25.1 s, 30 request(s), still running; cause Outage { line: "We’re currently experiencing high demand, which may cause temporary errors." }; stream failure Some("We’re currently experiencing high demand, which may cause temporary errors.")
- **P-tui** PASS: DECSET 2004 at boot: true; first screen ["  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings. Folder settings", "  can run code automatically, even without a model request. Continue only if you trust these files. Your trust", "  decision will be saved.", "› 1. Trust and continue", "  2. Quit", "  enter continue · esc quit"]; boot dialog "› 1. Trust and continue 2. Quit" (answered with "\r") and dismissed; bracketed paste + CR submitted: on the first paste; output quiet 1500 ms after the turn: true; `/mcp` screen names marion: true

### copilot 1.0.89

- **P-version** PASS: 1.0.89 (admitted in PINNED_HARNESSES); no-self-update env COPILOT_AUTO_UPDATE=false carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `marion-report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 4.9 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage none (row: no usage rule) (provider sent [(111, 10, 5), (211, 17, 10)]); session bf2d6ec4-e99f-4512-8dde-8bded0cdd28b ; activity marion-report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("Permission denied and could not request permission from user")), 0 permission ask(s) reached marion
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): S31 p0b/copilot (1.0.83): `-p … --resume=<sessionId>` continues the session
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (signal 2); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session 6466ab16-62b2-42fe-aed4-472c6b1408a7 -> 6466ab16-62b2-42fe-aed4-472c6b1408a7; resume refusal: none
- **P-lifecycle** FAIL: turn end: exit 0, left []; SIGTERM mid-turn: signal 15, left [(11936, "<NPM_PREFIX>/node_modules/@github/copilot-darwin-arm64/copilot -p CONFLIFETERM: follow the conformance script.\\012\\012When you have finished, call the `report` tool of the marion MCP server exactly once, with a one-sentence `narrative` of what you did. Your final message is not returned to whoever delegated this task; only that report is. --output-format json -C <SCRATCH>/copilot/p-lifecycle-sigterm/repo --disable-builtin-mcps --no-custom-instructions --model marion-canned --available-tools=marion-report,marion-spawn,marion-status,marion-wait,marion-list,marion-steer,marion-cancel,view,create,edit,apply_patch,bash --allow-tool=marion(report) --allow-tool=marion(spawn) --allow-tool=marion(status) --allow-tool=marion(wait) --allow-tool=marion(list) --allow-tool=marion(steer) --allow-tool=marion(cancel) --allow-tool=write --allow-tool=shell --additional-mcp-config @<SCRATCH>/copilot/p-lifecycle-sigterm/node/mcp.json"), (12946, "<WORKTREE>/target/debug/marion-supervisor mcp")]
- **P-errors** PASS: 401: ended after 2.8 s, 3 request(s), exit 1; cause Auth { line: "{\"code\":\"invalid_api_key\",\"message\":\"Incorrect API key provided: ******.\",\"type\":\"invalid_request_error\"}" }; stream failure Some("Authentication failed with provider at http://127.0.0.1:50217/v1 (HTTP 401).\n  Check your COPILOT_PROVIDER_API_KEY, COPILOT_PROVIDER_API_KEY_COMMAND, or COPILOT_PROVIDER_BEARER_TOKEN."); refused credential read off a frame: "{\"code\":\"invalid_api_key\",\"message\":\"Incorrect API key provided: ******.\",\"type\":\"invalid_request_error\"}" | 429: still running after 45.0 s, 5 request(s), still running; cause RateLimit { line: "{\"code\":\"rate_limit_exceeded\",\"message\":\"rate limit exceeded\",\"type\":\"rate_limit_exceeded\"}" }; stream failure None | 500: ended after 14.9 s, 6 request(s), exit 1; cause Outage { line: "{\"code\":null,\"message\":\"internal server error\",\"type\":\"server_error\"}" }; stream failure Some("Failed to get response from the AI model; retried 5 times (total retry wait time: 14.00 seconds) Last error: 500 internal server error")
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### goose 1.55.0

- **P-version** PASS: 1.55.0 (not listed in the row's verified releases); binary never updates itself (1.49.0 never updates itself on `run`/`session`: no update check in the binary's strings, no `GOOSE_*` update variable; `goose update` is explicit only)
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 4.4 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 200, output: 40, cache_read: 20, cache_write: 0, reasoning: None } (provider sent [(111, 10, 5), (211, 17, 10), (311, 24, 15)]); session none (row: no session rule) ; activity marion__report
- **P-approval** UNSUPPORTED: the row declares approve-all: every tool runs without asking (auto runs every tool the node is offered, which under --no-profile is marion's and the declared builtins, unasked. S26 on 1.49.0: GOOSE_MODE=approve aborts at exit 1 after the toolRequest and chat withholds every call. S37 on 1.52.0 (s37-goose-mode): unset, a fresh config runs marion's tool and a granted write unasked, and conformance's P-approval ran `report` with the variable stripped, so no grant of marion's is load-bearing. The row's env states GOOSE_MODE=auto because `GOOSE_MODE: approve` in the operator's config.yaml aborts a headless run at exit 1 and the env overrides it)
- **P-midturn** UNSUPPORTED: headless delivery None { note: "S31 p0b/goose (1.51.0): `run --resume -n <name>` continues a session by a name the caller chooses, which needs this row's --no-session dropped; until it is, there is no second turn" }
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (exit 1); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** UNSUPPORTED: the row states no resume spelling (`resume: None`)
- **P-lifecycle** PASS: turn end: exit 0, left []; SIGTERM mid-turn: signal 15, left [] (each after marion's end of the node)
- **P-errors** PASS: 401: ended after 0.1 s, 2 request(s), exit 1; cause Auth { line: "Error: Ran into this error: Authentication error: Authentication failed for http://127.0.0.1:52006/v1/chat/completions. Status: 401 Unauthorized. Response: Incorrect API key provided: dummy.." }; stream failure Some("Ran into this error: Authentication error: Authentication failed for http://127.0.0.1:52006/v1/chat/completions. Status: 401 Unauthorized. Response: Incorrect API key provided: dummy..\n\nPlease retry if you think this is a transient or recoverable error."); refused credential read off a frame: "Ran into this error: Authentication error: Authentication failed for http://127.0.0.1:52006/v1/chat/completions. Status: 401 Unauthorized. Response: Incorrect API key provided: dummy..\n\nPlease retry if you think this is a transient or recoverable error." | 429: ended after 6.9 s, 8 request(s), exit 1; cause RateLimit { line: "Error: Ran into this error: Rate limit exceeded: rate limit exceeded." }; stream failure Some("Ran into this error: Rate limit exceeded: rate limit exceeded.\n\nPlease retry if you think this is a transient or recoverable error.") | 500: ended after 6.8 s, 8 request(s), exit 1; cause Outage { line: "Error: Ran into this error: Server error: Server error (500 Internal Server Error) at http://127.0.0.1:52097/v1/chat/completions: internal server error." }; stream failure Some("Ran into this error: Server error: Server error (500 Internal Server Error) at http://127.0.0.1:52097/v1/chat/completions: internal server error.\n\nPlease retry if you think this is a transient or recoverable error.")
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### opencode 1.18.33

- **P-version** PASS: 1.18.33 (admitted in PINNED_HARNESSES); no-self-update env OPENCODE_DISABLE_AUTOUPDATE=1 carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `marion_report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 6.0 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 180, output: 40, cache_read: 20, cache_write: 0, reasoning: Some(10) } (provider sent [(111, 10, 5), (211, 17, 10)]); session ses_f0bb24977ffe5wqmIEBdyRcZnc ; activity marion_report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("The user rejected permission to use this specific tool call.")), 0 permission ask(s) reached marion
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): S31 p0b/opencode (1.18.32): `run --session <id>` continues the session with the store in a file OPENCODE_DB (db1/db2); `:memory:` failed with Session not found
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (signal 2); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session ses_f0bb2306effedWXL2kvkHpjaS5 -> ses_f0bb2306effedWXL2kvkHpjaS5; resume refusal: none
- **P-lifecycle** PASS: turn end: exit 0, left []; SIGTERM mid-turn: signal 15, left []
- **P-errors** FAIL: 401: ended after 2.4 s, 1 request(s), exit 1; cause Auth { line: "Incorrect API key provided: dummy." }; stream failure Some("Incorrect API key provided: dummy."); refused credential read off a frame: "Incorrect API key provided: dummy." | 429: still running after 45.0 s, 5 request(s), still running; cause none said; stream failure None | 500: still running after 45.0 s, 5 request(s), still running; cause none said; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### pi 0.99.1

- **P-version** PASS: 0.99.1 (admitted in PINNED_HARNESSES); no-self-update env PI_SKIP_VERSION_CHECK=1 carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; still running; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `mcp__marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 4.7 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 180, output: 40, cache_read: 20, cache_write: 0, reasoning: None } (provider sent [(111, 10, 5), (211, 17, 10)]); session 01a0f45f-abdf-73a0-96ea-f4242d614c15 ; activity mcp__marion__report
- **P-approval** UNSUPPORTED: the row declares approve-all: every tool runs without asking (S34 on 0.80.2: pi asks nothing headless; a tool named in --tools runs, and marion's verb reaches the bridge with no prompt)
- **P-midturn** PASS: folded into the running turn (1 turn end; requests [3])
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (still running); 1 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session 01a0f45f-f44c-739b-8835-bad2b5da2b6e -> 01a0f45f-f44c-739b-8835-bad2b5da2b6e; resume refusal: none
- **P-lifecycle** PASS: idle stdin EOF: exit 0, left []; SIGTERM mid-turn: exit 143, left []
- **P-errors** PASS: 401: ended after 0.4 s, 1 request(s), still running; cause Auth { line: "401: {\"code\":\"invalid_api_key\",\"message\":\"Incorrect API key provided: dummy.\",\"type\":\"invalid_request_error\"}" }; stream failure Some("401: {\"code\":\"invalid_api_key\",\"message\":\"Incorrect API key provided: dummy.\",\"type\":\"invalid_request_error\"}"); refused credential read off a frame: "401: {\"code\":\"invalid_api_key\",\"message\":\"Incorrect API key provided: dummy.\",\"type\":\"invalid_request_error\"}" | 429: ended after 14.5 s, 4 request(s), still running; cause RateLimit { line: "429: {\"code\":\"rate_limit_exceeded\",\"message\":\"rate limit exceeded\",\"type\":\"rate_limit_exceeded\"}" }; stream failure Some("429: {\"code\":\"rate_limit_exceeded\",\"message\":\"rate limit exceeded\",\"type\":\"rate_limit_exceeded\"}") | 500: ended after 14.8 s, 4 request(s), still running; cause Outage { line: "500: {\"code\":null,\"message\":\"internal server error\",\"type\":\"server_error\"}" }; stream failure Some("500: {\"code\":null,\"message\":\"internal server error\",\"type\":\"server_error\"}")
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)

### qwen 0.24.7

- **P-version** PASS: 0.24.7 (admitted in PINNED_HARNESSES); no-self-update env QWEN_CODE_SKIP_UPDATE_CHECK_ONCE=true carried
- **P-launch** PASS: turn ended; provider asked for the marker true on {"openai"}; exit 0; declaration route verified at compile; auth failure line: none
- **P-tools** PASS: first request lists `mcp__marion__report`: true; with the bridge 4 s slow and no gate of marion's, the first request came 5.0 s after the spawn and listed marion's tools
- **P-activity** PASS: turn ended; report Some(Answered); narrative Some("conformance CONFACTIVITY"); usage TokenUsage { input: 200, output: 40, cache_read: 20, cache_write: 0, reasoning: None } (provider sent [(111, 10, 5), (211, 17, 10)]); session ec6be2d0-8daa-493f-a0e4-5597e8eed775 ; activity mcp__marion__report
- **P-approval** PASS: granted: `report` answered true; ungranted: `report` Some(Refused("Qwen Code requires permission to use \"mcp__marion__report\", but that permission was declined (non-interactive mode cannot prompt for confirmation).")), 0 permission ask(s) reached marion
- **P-midturn** UNSUPPORTED: headless delivery is Continuation (a relaunch per turn, no mid-turn channel): S31 p0b/qwen (0.23.0): `--resume <session_id> -p …` continues the session
- **P-interrupt** PASS: the cancel ended the turn in 0.0 s (exit 130); 0 descendant(s) alive after it; marion's kill sweep confirmed the node dead and left []
- **P-resume** PASS: second life ended; its request carries the first life's prompt: true; session ff0a0907-a733-455c-8601-8702bc1d3b31 -> ff0a0907-a733-455c-8601-8702bc1d3b31; resume refusal: none
- **P-lifecycle** PASS: turn end: exit 0, left []; SIGTERM mid-turn: exit 130, left []
- **P-errors** FAIL: 401: ended after 1.0 s, 1 request(s), exit 1; cause Auth { line: "[API Error: 401 Incorrect API key provided: dummy.]" }; stream failure Some("error_during_execution"); refused credential read off a frame: "[API Error: 401 Incorrect API key provided: dummy.]" | 429: still running after 45.0 s, 20 request(s), still running; cause none said; stream failure None | 500: still running after 45.0 s, 20 request(s), still running; cause none said; stream failure None
- **P-tui** UNSUPPORTED: the row has no pane shape (`pane: None`)
