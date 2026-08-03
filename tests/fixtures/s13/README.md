# S13 — what an opencode adapter needs, and why §6.4's opencode bullet describes the wrong path

Run 2026-08-03 against **opencode 1.17.3** (`/opt/homebrew/Cellar/opencode/1.17.3/bin/opencode`, a
114 MB Mach-O arm64 executable — a bun-compiled single file with the JS embedded), macOS
(darwin 25.5.0, arm64), against a **canned local OpenAI-compatible provider** on `127.0.0.1`.
**No model was called, no paid token was spent, no real credential was read.**
**Total spend: $0.00.**

Answers the **opencode half of design doc §11 item 10** — the opencode launcher findings §6.4
records had no committed fixture. **Not a close of item 10**: the server-path claims that item names
(`prompt_async`, `/event` vs `/api/event`, `OPENCODE_EXPERIMENTAL_EVENT_SYSTEM`) were **not
re-measured here**, because this spike deliberately measured a different surface. See *Still open*.

## ANSWER: the headless path binds no port, needs none of §6.4's three settings, and never times out on its own.

`opencode run --pure --format json` is a full agent turn on stdout as NDJSON, over pipes, with **no
TCP listener at all**. It is the path a marion adapter should compile to, and none of §6.4's
opencode bullet — `POST /session/{id}/prompt_async`, `/event` vs `/api/event`,
`OPENCODE_EXPERIMENTAL_EVENT_SYSTEM=true` — applies to it. Those three describe `opencode serve`,
which remains the right text for a future ACP/server surface and is not retracted here.

Three findings dominate the adapter work:

- **`XDG_CONFIG_HOME` is the only true config isolation.** `OPENCODE_CONFIG` and
  `OPENCODE_CONFIG_CONTENT` both **merge on top of the user's global config** — a child launched
  with either still loads the operator's MCP servers.
- **opencode never exits on a provider hang.** A 500 was still retrying at 90 s; a
  connection-refused was still hung at 180 s. No bounded backoff ceiling was found.
- **opencode reads `~/.claude/CLAUDE.md` and `~/.claude/skills/**` by default.** A marion-spawned
  opencode child silently adopts a *different harness's* user configuration, defeating
  `inherit_user_config: false` by a route that default never contemplated.

The one place opencode is **better** than its peers: MCP tool permissions default to **allow**, so
there is no silent-omission trap of the `default_tools_approval_mode` / `trust: true` family here.

## What is here

**Nothing but this file.** No capture file is committed. This spike was **binary and strings
inspection of the installed executable plus short live probes** against a canned local endpoint; it
produced no data set. Every claim below is either a quoted line lifted out of the bundled JS, a
quoted probe output, or a measurement stated with its numbers. Saying that plainly is the point —
there is no `tests/fixtures/s13/*.jsonl` to go look at, and this file must not be read as if there
were. Same deviation from the every-spike-emits-a-fixture rule that S12 declared, for the same
reason.

## Headless invocation

`opencode run [message..]`. The argv that produced a full agent turn including an MCP tool call:

```sh
opencode run --pure --format json --title "marion-child" "your prompt here"
```

- The prompt is a **positional variadic** argument (multiple words are joined) **or** stdin —
  `echo "say hi" | opencode run` works identically. With **neither**, it exits **1** with
  `Error: You must provide a message or a command`.
- **It does not require a tty and it does not refuse one.** Verified both with `< /dev/null` and
  under `script -q /dev/null`: **identical NDJSON on stdout**. `--format json` is not tty-gated;
  only the `default` renderer branches on `process.stdout.isTTY`. **This is the direct contrast with
  S11**, where `claude -p` exits 1 on a pty stdin.
- `--interactive` (default false) is the opt-in TUI-in-`run` mode. Leave it off.
- Cold start with a brand-new `HOME`: **~1.4 s** wall.
- **`--title "x"` suppresses the extra title-generation model call.** Measured: **2 POSTs with
  `--title`, 3 without**; the extra is a `You are a title generator` request against `small_model`.
  Pass it. (Same shape of finding as §5.5's Claude Code session-title request, which cost a run
  there.)

## Output: NDJSON, six event types, and no terminal frame

`--format json` (choices `default|json`) writes one JSON object per line to stdout. The emitter,
from the bundled run handler:

```js
function v(ED,JD){ if(D.format==="json") return process.stdout.write(JSON.stringify({type:ED,timestamp:Date.now(),sessionID:e,...JD})+SF),!0; return !1 }
```

The **complete** set of `type` values is `step_start`, `step_finish`, `text`, `reasoning`,
`tool_use`, `error`. Every line carries `{type, timestamp, sessionID, …}`.

**There is no init, result or usage summary event.** The stream simply ends when
`session.status === "idle"`. **A reader must terminate on stdout close, not on a terminal frame** —
the contrast with codex and gemini, which both emit one.

Captured events (ids redacted per house style):

```json
{"type":"tool_use","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"type":"tool","tool":"marionmcp_report","callID":"call_1","state":{"status":"completed","input":{"text":"hello-from-marion"},"output":"MCP_CALLED {\"text\": \"hello-from-marion\"}","metadata":{"truncated":false},"title":"","time":{"<REDACTED-timestamps>":0}},"id":"<PART-1>","sessionID":"<SESSION-1>","messageID":"<MESSAGE-1>"}}
{"type":"text","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"id":"<PART-2>","messageID":"<MESSAGE-1>","type":"text","text":"CANNED_OK","time":{"start":"<TS>","end":"<TS>"}}}
{"type":"step_finish","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"reason":"stop","type":"step-finish","tokens":{"input":0,"output":0,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0}}
{"type":"error","timestamp":"<TS>","sessionID":"<SESSION-1>","error":{"name":"APIError","data":{"message":"bad request","statusCode":400,"isRetryable":false,"responseHeaders":{"<REDACTED-machine-specific>":""},"responseBody":"<REDACTED>","metadata":{"url":"<REDACTED>"}}}}
```

Two framing properties a reader must not assume away:

- **`tool_use` fires only on terminal states** (`completed` / `error`). There are no streaming
  partials.
- **`text` parts are emitted whole**, on `time.end`, not token-by-token.

## MCP injection: config-only, and neither env path isolates

**There is no CLI flag for MCP servers.** Two env paths inject one without touching the user's real
file — **but neither replaces the user's config**:

| var | what it is | isolates? |
| --- | --- | --- |
| `OPENCODE_CONFIG` | a file **path** | **no** — merges on top of the global config (verified: the user's own MCP servers still loaded alongside) |
| `OPENCODE_CONFIG_CONTENT` | inline JSONC **text**, a virtual source, never written back | **no** — also merges |
| `OPENCODE_CONFIG_DIR` | an extra config **directory**, appended to the search list | no |
| `XDG_CONFIG_HOME` | relocates the global config to `$XDG_CONFIG_HOME/opencode/` | **yes — the only true replacement** |

**So to point a child at a generated config without inheriting the user's, marion MUST set
`XDG_CONFIG_HOME`.** Verified with `opencode debug paths`, which reported `config /…/sb/config/opencode`.

Search order, all **merged**, later wins:

```
$XDG_CONFIG_HOME/opencode/{config.json,opencode.json,opencode.jsonc}
  -> $OPENCODE_CONFIG
  -> project opencode.json(c) walk-up
  -> .opencode/ dirs
  -> $OPENCODE_CONFIG_DIR
  -> managed dir
  -> OPENCODE_CONFIG_CONTENT
```

The macOS managed/MDM dir is `/Library/Application Support/opencode` plus
`/Library/Managed Preferences/[user/]ai.opencode.managed.plist` — **neither exists on this
machine** — and is overridable with `OPENCODE_TEST_MANAGED_CONFIG_DIR`.

The exact local-stdio schema, from `https://opencode.ai/config.json`, `$defs.McpLocalConfig`,
`additionalProperties: false`:

```json
{"mcp":{"marionmcp":{"type":"local","command":["python3","/abs/path/server.py","--flag"],"cwd":"/optional/dir","environment":{"MARION_TOKEN":"abc123"},"enabled":true,"timeout":5000}}}
```

`type` is required, enum `["local"]`. **`command` is required and is an ARGV ARRAY, not a string.**
`environment` is `Record<string,string>`. `timeout` is ms, default 5000. The remote variant is
`{"type":"remote","url":…,"headers":{…},"oauth":false,"enabled":true,"timeout":N}`. Config values
support `{env:VAR}` and `{file:path}` substitution, so a per-node secret need not be embedded in the
generated JSON.

**Tool naming: `<serverName>_<toolName>`.** Verified live — `spawn` and `report` surfaced to the
model as `marionmcp_spawn` / `marionmcp_report`, while the JSON-RPC `tools/call` went out with the
**unprefixed** name `report`. Negotiated `protocolVersion "2025-11-25"`, `clientInfo
{"name":"opencode","version":"1.17.3"}`. Full round trip verified end to end. This is a **third**
spelling convention alongside Claude Code's `mcp__marion__report` and gemini's `mcp_marion_report`
(and codex's `{name,namespace}` dispatch form) — §5.4's per-harness-spelling rule holds.

## HOME / auth isolation — VERDICT: COPYABLE, and injectable inline

**There is no single `CODEX_HOME` analogue.** Path resolution is pure XDG:

```js
Q = env.XDG_DATA_HOME   || join(home, ".local", "share")
V = env.XDG_CONFIG_HOME || join(home, ".config")
p = env.XDG_STATE_HOME  || join(home, ".local", "state")
i = env.XDG_CACHE_HOME  || join(home, ".cache")
```

`opencode debug paths` confirms config, data, cache and state all relocate. `home` is
`os.homedir()`, overridable **independently** by `OPENCODE_TEST_HOME`. **So marion must set all
four XDG vars plus `HOME`** — `Path.home` drives the `~/.claude`, `~/.agents` and `~/.opencode`
lookups regardless of XDG.

**Credentials are files, not Keychain**, as with Codex (S8) and unlike Claude Code:

```
$ security find-generic-password -s opencode
The specified item could not be found in the keychain.
```

A keychain dump grepped for `opencode` returns nothing. Storage:

- `~/.local/share/opencode/auth.json` (mode 600) — v1 shape, keyed by provider:
  `{"github-copilot":{"type":"oauth","refresh":…,"access":…,"expires":…},"google":{"type":"api","key":…}}`
- `~/.local/share/opencode/account.json` (mode 600) — v2 shape:
  `{version:2, accounts:{<id>:{id,serviceID,description,credential}}, active:{<provider>:<id>}}`

**The best lever is `OPENCODE_AUTH_CONTENT`** — inline JSON (v1 or v2; v1 is auto-migrated), a
whole-file replacement with no copy at all:

```js
if (process.env.OPENCODE_AUTH_CONTENT) try { return JSON.parse(process.env.OPENCODE_AUTH_CONTENT) } catch(D) {}
```

opencode itself uses this to propagate credentials into child workspaces.

**The tradeoff, stated rather than sold.** This puts a live credential in the child's
**environment**, which is readable by `ps -E`-class inspection under the same uid and is **inherited
by grandchildren**. That is precisely the reasoning §7.1/§9 already applied to Codex — which is why
codex's MCP declaration lives in `config.toml` rather than in `-c` argv. **A file copy under a
`0700` agent dir may be the better choice for marion despite being less convenient**, and this
fixture does not recommend the env path by default.

## Model and provider: an arbitrary OpenAI-compatible base URL works, at zero cost

Precedence: `-m/--model provider/model` **>** config `model` key. **There is no `OPENCODE_MODEL` env
var** (exhaustive `OPENCODE_*` scan of the binary). Also present: `--variant` (reasoning effort),
`--agent`, config `small_model` (title/summary tasks) and `default_agent`.

`@ai-sdk/openai-compatible` is **bundled in the binary**, so no npm fetch is needed to use it. The
exact config that worked:

```json
{"model":"fake/fake-1","small_model":"fake/fake-1","provider":{"fake":{"npm":"@ai-sdk/openai-compatible","name":"Fake","options":{"baseURL":"http://127.0.0.1:8919/v1","apiKey":"sk-fake"},"models":{"fake-1":{"name":"Fake One","tool_call":true}}}}}
```

Confirmed on the wire: `POST http://127.0.0.1:8919/v1/chat/completions`,
`Authorization: Bearer sk-fake`, `stream: true`, standard OpenAI `tools[].function` schema, standard
SSE `chat.completion.chunk` deltas **including streamed `tool_calls`**. The keys are
`provider.<id>.options.baseURL` / `.apiKey` — **camelCase `baseURL`**. A provider may declare
`env: ["SOME_KEY"]` to pull its key from the environment. `options` also carries `timeout`,
`headerTimeout` and `chunkTimeout` (ms, or `false`).

Also: `disabled_providers` / `enabled_providers` arrays, and **`OPENCODE_DISABLE_MODELS_FETCH=1`**,
which stops the models.dev catalogue fetch that otherwise runs at boot **and repeats on a 60-minute
in-process loop**.

## Tool permissions — the negative result: there is no silent trap here

Config key `permission`, or `OPENCODE_PERMISSION='{…}'` as inline JSON merged over config. Values
are `"ask" | "allow" | "deny"`, or `{pattern: action}`, for `read`/`edit`/`glob`/`grep`/`list`/
`bash`/`task`/`external_directory`/`lsp`/`skill`. `additionalProperties` accepts **any** tool name,
including `marionmcp_report`. The legacy `tools: {name: bool}` form maps onto allow/deny.

**The default for MCP tools is ALLOW.** Verified: with no `permission` entry for `marionmcp_*` the
tool executed with no prompt and no blocking. **MCP calls are not silently omitted or cancelled, and
marion needs no allowlist for basic function.**

Measured matrix, all non-interactive with stdin closed:

| `permission` for `marionmcp_report` | result |
| --- | --- |
| unset | tool present, runs immediately |
| `"allow"` | same |
| `"ask"` | stderr `! permission requested: marionmcp_report (*); auto-rejecting`; the tool part becomes `{"status":"error","error":"The user rejected permission to use this specific tool call."}`; **the run continues, exit 0**; it does **not** hang |
| `"deny"` | the tool is **removed from the model's tool list entirely** (tools seen dropped 8 → 7) |
| `"ask"` + `--dangerously-skip-permissions` | `{"status":"completed"}` — auto-approved |

The run loop confirms it: `if(D["dangerously-skip-permissions"])` reply `"once"`, else
`println("…auto-rejecting")` and reply `"reject"`.

Note that **`"deny"` is strictly better than `"ask"`** for a tool marion wants gone: `deny` removes
it from the schema, while `ask` leaves it advertised and wastes a model turn getting rejected.

This is worth recording as a **negative result against §12's silent-failure family**. codex has
`default_tools_approval_mode`, gemini has `trust: true`; **opencode has neither, and its default is
the working one.** The family should not be over-generalised into "every harness has one".

## Session and exit codes — the headline hazard

`sessionID` (`ses_` + 26 chars) is on **every** event and therefore available from the **first
line**. Resume with `-s/--session <id>` or `-c/--continue`; `--fork` branches instead of appending.
`opencode session list`, `opencode session delete <id>`, and `opencode export <id> [--sanitize]`
exist — the last prints `Exporting session: <id>` to stdout **before** the JSON, so line 1 must be
stripped.

**Measured exit codes:**

| condition | exit | what appears |
| --- | --- | --- |
| success | **0** | normal events |
| no message and no stdin | **1** | `Error: You must provide a message or a command` on stderr |
| unknown flag | **1** | yargs usage on **stdout** |
| unresolvable model (`-m nope/nope`) | **1** | `Error: {"name":"UnknownError",…}` on stderr |
| provider returns 400 (non-retryable) | **1** | one `{"type":"error",…}` line on stdout; **stderr empty** |
| provider returns 500 | **never exits** | still retrying at **90 s** |
| provider connection-refused | **never exits** | still hung at **180 s** |

**marion MUST impose its own wall-clock timeout and process-group kill for an opencode child.** No
bounded backoff ceiling was found. `provider.<id>.options.timeout` / `headerTimeout` are a first
line of defence but not a substitute. **This makes §9's two-step group kill load-bearing for
opencode rather than optional** — for opencode the trigger is a hang the harness will never resolve
on its own, not merely a slow turn.

## Background processes, plugins, and what `--pure` does not gate

**(a) A detached npm install that `--pure` does not gate.** For every config directory,
`Config.loadInstanceState` does:

```js
let P = yield* A.install(J,{add:[{name:"@opencode-ai/plugin",version:…}]}).pipe(…, W.forkDetach)
```

`forkDetach` means "not interrupted by scope close", and there is **no `pure` guard on it**. The
install runs **in-process** via npm's Arborist (`ignoreScripts: true`, `binLinks: true`, registry
from `.npmrc`) under a file lock in `<state>/locks`, so this path spawns no orphan OS process.
Empirically: a cold `--pure` run in a virgin `HOME` finished in **1.45 s** and created **7.4 MB** of
`$HOME/.npm/_cacache` but **never materialised `node_modules`** — it starts a network fetch to
`registry.npmjs.org` and the process exits mid-flight. Under a longer-lived run it completes: a
working sandbox accumulated **61 MB** of `config/opencode/node_modules` plus **87 MB** of `~/.npm`.
Mitigation: point `XDG_CONFIG_HOME` and `HOME` at throwaway dirs and **expect first-run network
access**, or pre-seed `node_modules` + `package-lock.json` so the dirty check short-circuits.
**This violates the same expectation the codex plugin-clone `git fetch` did (§6.4, §12): a run
specified to make no network call makes one.**

It also writes a `.gitignore` (`node_modules`, `package.json`, `package-lock.json`, `bun.lock`,
`.gitignore`) into **every config dir it touches, including project `.opencode/` dirs**. Suppress
with `OPENCODE_DISABLE_PROJECT_CONFIG=1`.

**(b) Git subprocesses.** `Git.clone/fetch/fetchBranch/checkout/reset` back the `references` feature
(cloning into `<data>/repos/<host>/<owner>/<repo>`), gated behind the `references` config key or
`OPENCODE_EXPERIMENTAL_REFERENCES` / `OPENCODE_EXPERIMENTAL`. **These are awaited, not detached.**
The genuinely leaky path is `plugin: ["foo@git+https://…"]`, which routes through the bundled
`pacote`, whose `GitFetcher` config (`npmInstallCmd: ["install","--force"]`) is present verbatim and
shells out to `git` and possibly `npm`. **UNKNOWN** whether `ignoreScripts` is threaded through to
pacote's prepare step. `--pure` skips loading external plugins, so this is avoided.

**(c) ripgrep auto-download.** If `rg` is not on `PATH`, opencode fetches
`github.com/BurntSushi/ripgrep/releases/download/15.1.0/….tar.gz` and spawns `tar -xzf` into
`<XDG_CACHE_HOME>/opencode/bin`. **Not gated by `--pure` and not by
`OPENCODE_DISABLE_LSP_DOWNLOAD`.** Pre-seed `rg` on the child's `PATH`.

**(d) LSP servers** are long-lived `--stdio` children. `OPENCODE_DISABLE_LSP_DOWNLOAD=1` blocks the
download/build step (`go install gopls@latest`, `gem install rubocop`, a zip fetch plus
`npm install && npm run compile` for eslint, `mix deps.get`/`compile` for elixir-ls) but **not the
spawn of an already-installed server**. **UNKNOWN:** no explicit kill or finalizer for LSP children
was found; they appear to rely on process exit.

**(e) Auto-updater.** It pipes `https://opencode.ai/install` into `bash` and can shell out to
`brew`/`npm`/`yarn`/`pnpm`/`bun`/`scoop`/`choco`. Gated by `autoupdate: false` or
`OPENCODE_DISABLE_AUTOUPDATE`; it only auto-applies patch-level bumps. The trigger function lives
**only in the desktop-app RPC bundle with no call site found in the CLI/TUI path**, so `opencode
run` likely never fires it. Set the flag anyway.

**(f) `opencode run` binds no TCP port.** It uses an in-process fetch handler
(`baseUrl: "http://opencode.internal"`, `fetch: Server.Default().app.fetch`). `Server.listen` has
exactly four call sites: `serve`, `acp`, `web`, and the desktop RPC. `--port` is *declared* on `run`
but appears **never consumed** — vestigial, **inferred from source and NOT confirmed with `lsof`**,
so it is marked UNKNOWN below. When a server *is* started it binds `127.0.0.1` by default **except
`--mdns`, which silently rewrites the default hostname to `0.0.0.0`**. `OPENCODE_SERVER_PASSWORD` is
optional — its absence only warns. Shutdown is clean (`gracefulShutdownTimeout "1 second"`, mDNS
unpublish as a scope finaliser).

**(g) `detached: true`** is used by the **bash tool** (`stdin: "ignore"`, `detached: true`,
`forceKillAfter` 3 s plus a timeout) for process-group kill semantics. **A bash grandchild that
escapes the group signal can outlive opencode** — the **same class** as the codex `setsid` tool-call
escape (§11 item 18) and the codex plugin-clone orphan.

**(h) sqlite** is in-process `bun:sqlite`, WAL, `busy_timeout 5000`, passive checkpoint at open.
**No daemon.** The path is overridable: `OPENCODE_DB=:memory:`, an absolute path, or a basename
relative to `<data>`.

**(i) models.dev** does an initial fetch plus a `repeat("60 minutes")` `forkScoped` loop — scoped,
so it dies with the process. Kill it with `OPENCODE_DISABLE_MODELS_FETCH=1`; override the source
with `OPENCODE_MODELS_URL` / `OPENCODE_MODELS_PATH`.

**Empirical:** after ~15 runs, `ps` showed **zero** leftover `opencode` or MCP child processes from
these tests.

## Cross-harness contamination — a hazard class this document has no text for

By default opencode reads:

- `~/.claude/CLAUDE.md`
- **every** `CLAUDE.md` between `cwd` and the worktree root
- `~/.claude/skills/**/SKILL.md`
- **every** project `.claude/skills/**/SKILL.md`

and it scans `~/.claude/ide/*.lock` to discover a running Claude Code IDE websocket bridge
(**UNKNOWN** whether any env var gates that scan).

**A marion-spawned opencode child inherits all of it** unless severed with
`OPENCODE_DISABLE_CLAUDE_CODE=1` and `OPENCODE_DISABLE_EXTERNAL_SKILLS=1`.

This is a **novel hazard class for marion**: one harness silently adopting a *different* harness's
user configuration. §6.4's `inherit_user_config: false` default is written against a harness reading
its **own** config dir; it does not contemplate opencode reading Claude Code's. The default is
therefore defeated by a route it never covered, and no amount of `XDG_*` isolation fixes it —
`~/.claude` is found via `HOME`, and `.claude/skills` via the project tree.

## The adapter's target invocation

Recorded verbatim as the shape a marion opencode launcher should compile to:

```sh
env -i PATH=… TMPDIR=… HOME=$SANDBOX \
  XDG_CONFIG_HOME=$SANDBOX/config XDG_DATA_HOME=$SANDBOX/data \
  XDG_CACHE_HOME=$SANDBOX/cache XDG_STATE_HOME=$SANDBOX/state \
  OPENCODE_AUTH_CONTENT="$(cat auth.json)" \
  OPENCODE_CONFIG_CONTENT='{"mcp":{"marion":{"type":"local","command":[…],"environment":{…}}}}' \
  OPENCODE_PERMISSION='{"bash":"deny","edit":"deny","webfetch":"deny"}' \
  OPENCODE_DISABLE_CLAUDE_CODE=1 OPENCODE_DISABLE_EXTERNAL_SKILLS=1 \
  OPENCODE_DISABLE_PROJECT_CONFIG=1 OPENCODE_DISABLE_MODELS_FETCH=1 \
  OPENCODE_DISABLE_LSP_DOWNLOAD=1 OPENCODE_DISABLE_AUTOUPDATE=1 \
  OPENCODE_DISABLE_SHARE=1 OPENCODE_DB=:memory: \
  opencode run --pure --format json --title marion-child -m fake/fake-1 "$PROMPT" < /dev/null
```

plus, on marion's side and not expressible in argv:

- its **own wall-clock timeout with a process-group kill** (the 500/connection-refused hangs above),
- `rg` pre-seeded on the child's `PATH` (else the ripgrep download fires),
- an NDJSON reader keyed on `sessionID` from **line 1**, terminating when **stdout closes**.

`OPENCODE_CONFIG_CONTENT` appears there **in addition to** `XDG_CONFIG_HOME`, not instead of it. It
carries the MCP declaration on top of an already-isolated config root; on its own it would merge
into the operator's real config and defeat the isolation entirely.

## Reproducing

No probe script is committed. The measurements above were made with the installed binary and a local
canned OpenAI-compatible endpoint; the load-bearing commands were:

```sh
opencode --version                                   # -> 1.17.3
opencode debug paths                                 # shows config/data/cache/state relocation
opencode run --pure --format json --title x "…"      # the measured headless turn
echo "say hi" | opencode run                         # stdin path
script -q /dev/null opencode run --format json "…"   # pty path -- identical NDJSON
security find-generic-password -s opencode           # -> item could not be found
```

Binary-source claims are readable straight out of the installed executable at
`/opt/homebrew/Cellar/opencode/1.17.3/bin/opencode` (bun-compiled, JS embedded — `strings` and a
grep over the extracted bundle). The MCP schema is from `https://opencode.ai/config.json`.
**Nothing here required a paid endpoint and nothing here cost money.**

## Redaction

- Session ids → `<SESSION-1>`, part ids → `<PART-1>`, message ids → `<MESSAGE-1>`, timestamps →
  `<TS>`, **numbered by first appearance** where correlation matters (following S9, S10 and S12
  rather than S6). The correlation this fixture records — that every event carries the *same*
  `sessionID`, available from line 1 — is destroyed by flattening.
- The `error` event's `responseHeaders`, `responseBody` and `metadata.url`, and the `tool_use`
  part's `state.time` map, are **reduced** to `"<REDACTED-…>"`; the keys are kept so the shape still
  reads. Nothing in them is evidence about this channel.
- No secret, token or credential value appears in this file. `~/.local/share/opencode/` was
  inspected for **filenames, modes and JSON shape only**, and the Keychain for **item existence
  only**. The provider `apiKey` quoted throughout is the literal string `sk-fake`.

## Still open after this

- **§6.4's three server-path claims were not re-measured.** `POST /session/{id}/prompt_async`, the
  `/event` vs `/api/event` envelope difference, and `OPENCODE_EXPERIMENTAL_EVENT_SYSTEM` describe
  `opencode serve`; S13 measured `opencode run` and says nothing about whether they are still true
  at 1.17.3. **§11 item 10's opencode half is therefore narrowed, not closed.**
- **Whether pacote's git-plugin path honours `ignoreScripts`** is unknown.
- **Whether LSP children get an explicit kill** is unknown; none was found in the binary.
- **Whether the `~/.claude/ide/*.lock` scan is gated by any env var** is unknown.
- **Whether `--port` on `run` truly does nothing** is *inferred from source* (declared but with no
  consumer found) and **was not confirmed with `lsof`**.
- **The detached npm install's full behaviour under a long-lived run was observed incidentally, not
  measured as a controlled case.** The 61 MB / 87 MB figures come from a working sandbox, not from a
  clean before/after.
- **No test exercised opencode as a marion child end to end.** This spike characterises the CLI
  only — there is no equivalent of M1's real-codex-child criterion for opencode.
- One machine, one CLI version, one canned provider, one run per configuration.
