# S12 — does `GEMINI_CLI_HOME` isolation break Gemini auth, and what does a Gemini adapter need

Run 2026-08-03 against **gemini CLI 0.53.0** (`/opt/homebrew/bin/gemini` →
`/opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/gemini.js`, a 94 MB esbuild bundle),
macOS (darwin 25.5.0, arm64), against a **local canned fake Gemini endpoint** on `127.0.0.1`.
**No model was called, no paid token was spent, no real credential was read**: every run used a
throwaway `GEMINI_API_KEY` with `GOOGLE_GEMINI_BASE_URL` pointed at the fake server.
**Total spend: $0.00.** The CLI's own docs ship inside the package at `bundle/docs/`, so the
documentary evidence below is from the installed artifact, not from the web.

Answers the **Gemini half of design doc §11 item 3** (left explicitly unmeasured by S8) and
fixtures several of the §6.4 **Gemini launcher claims** that §11 item 10 lists as having no
committed fixture. **Not a close of item 3** — the OAuth half is unverified; see *Still open*.

## ANSWER: isolation does break auth, and Gemini is the *easy* case — COPYABLE.

`GEMINI_CLI_HOME` relocates **everything**, credentials included, and nothing in that set is
unfixable-by-copy on the same machine under the same user. Gemini is **strictly better than Claude
Code** and no worse than Codex: there is no OS-held secret that a copy cannot reproduce, because
Gemini's file-backed credential store derives its key from `hostname + username` and a **hardcoded
passphrase**. There is also a clean sidestep — `GEMINI_API_KEY` — that Claude Code and Codex do not
offer on the subscription path.

**This corrects §6.4 and §11 item 3(c).** Both warn that "a generic-password Keychain item for
`service=gemini` **does** exist, so Gemini may behave like Claude Code." It does not. That item's
`acct` is **`antigravity`** — the Antigravity IDE, which shares `~/.gemini/` with the CLI. The CLI's
own Keychain service name is `gemini-cli-oauth`, and **no such item exists on this machine**:

```
$ security find-generic-password -s "gemini-cli-oauth"
security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.
```

`-s gemini-cli`, `-s "Gemini CLI"` and `-s google` are equally absent. Only item **existence** was
probed; no secret was read.

## What is here

**Nothing but this file.** This spike read the shipped bundle and ran short probes against a fake
endpoint; it produced no capture worth committing that is not quoted verbatim below, and the two
artifacts it did touch (`~/.gemini/`, the login Keychain) cannot be committed at all. That is a
deviation from the every-spike-emits-a-fixture rule and it is stated rather than hidden: the
reproduction commands are in *Reproducing*, and every claim below is either a quoted bundle source
line, a quoted probe output, or a measurement stated with its numbers.

## `GEMINI_CLI_HOME` is the relocation, and it relocates everything

From the bundle's `packages/core/src/utils/paths.ts`:

```js
var GEMINI_DIR = ".gemini";
function homedir() {
  const envHome = process.env["GEMINI_CLI_HOME"];
  if (envHome) return envHome;
  return os.homedir();
}
```

Every path routes through that one function. `Storage.getGlobalGeminiDir()` is
`join(homedir(), ".gemini")`, and under it live `settings.json`, `oauth_creds.json`,
`google_accounts.json`, `installation_id`, `trustedFolders.json`, `mcp-oauth-tokens.json`,
`a2a-oauth-tokens.json`, the session/chat directories, and **extensions**.

**Measured:** with `GEMINI_CLI_HOME` set, a fresh `$DIR/.gemini/` was created and **nothing was
written to the real `~/.gemini`**. Note the doubling — the CLI appends `.gemini` itself, so
`GEMINI_CLI_HOME=$SANDBOX` yields `$SANDBOX/.gemini/`.

There is **no `GEMINI_DIR` or `GEMINI_CONFIG_DIR` env var**; `GEMINI_DIR` is an internal constant
holding the literal `".gemini"`. `docs/cli/enterprise.md`, under *"User isolation in shared
environments"*, endorses exactly this pattern — so isolation is a supported use, not a hack.

Because extensions load from `<home>/.gemini/extensions`, **a clean sandbox home means zero
extensions**, with no flag needed. `-e/--extensions` narrows the set explicitly if one is seeded.

## Credential storage is hybrid, and the file half is copy-friendly

`HybridTokenStorage.initializeStorage()` probes a native keychain first — `@github/keytar`, a real
`.node` binary bundled at `bundle/node_modules/@github/keytar/build/Release/keytar.node` — with a
set/get/delete round trip plus `security default-keychain` on darwin, under a **2 s timeout**, and
falls back to `FileKeychain` if any of that fails. `GEMINI_FORCE_FILE_STORAGE=true` (the constant
`FORCE_FILE_STORAGE_ENV_VAR`) forces the file path unconditionally.

`FileKeychain` is the escape hatch, and this is why the verdict is COPYABLE:

```js
this.tokenFilePath = path.join(homedir(), GEMINI_DIR, "gemini-credentials.json");
deriveEncryptionKey() {
  const salt = `${os.hostname()}-${os.userInfo().username}-gemini-cli`;
  return crypto.scryptSync("gemini-cli-oauth", salt, 32);  // aes-256-gcm
}
```

The key derives **deterministically from hostname + username with a hardcoded passphrase**. No OS
secret participates. Any process running as the same user on the same host can decrypt the file,
and the file honours `GEMINI_CLI_HOME`. **Same machine + same user is the whole precondition** —
this fixture claims nothing about moving a profile between hosts or users, where the salt changes
and the copy stops decrypting.

A second flag, `GEMINI_FORCE_ENCRYPTED_FILE_STORAGE`, exists for the separate MCP-OAuth token store.

**One-way migration exists and is destructive.** `OAuthCredentialStorage.migrateFromFileStorage()`
reads the legacy `join(homedir(), ".gemini", "oauth_creds.json")`, writes it into the hybrid store,
then `fs.rm`s the original. A launcher that copies `oauth_creds.json` into a sandbox home must
expect the child to consume and delete its copy — which is fine for a copy, and would not be fine
for a symlink (cf. §11 item 3(b) for Codex).

## `~/.gemini/` on this machine — filenames and sizes only

No value was read from any of these:

```
oauth_creds.json     1.5K  mode 600      settings.json        528B
google_accounts.json   55B                settings.json.orig   130B
projects.json         843B                state.json           167B
trustedFolders.json   195B  mode 600      installation_id       36B
GEMINI.md             1.6K
dirs: agents/ config/ context-mode/ history/ skills/ tmp/
plus Antigravity IDE directories sharing the same folder
```

**No `gemini-credentials.json` is present** — this profile has never migrated to the hybrid store,
so its credential still sits in the legacy `oauth_creds.json`.

## The API-key sidestep, and the trap in it

Auth types are `oauth-personal`, `gemini-api-key`, `vertex-ai`, `compute-default-credentials`.
`GEMINI_API_KEY` (AI Studio) and `GOOGLE_API_KEY` (Vertex express) both work — which means a marion
node can avoid the whole credential-copy question by not using the subscription path at all.

**An API key alone is not enough.** A first run with only `GEMINI_API_KEY` set failed:

```json
{"error":{"type":"Error","message":"Invalid auth method selected.","code":41}}
```

Settings must *also* carry `{"security":{"auth":{"selectedType":"gemini-api-key"}}}`. With that
added it worked immediately. **This confirms §6.4's Gemini launcher claim**, which until now had no
fixture (§11 item 10).

## MCP injection: four layers, and the system layer wins

MCP servers are declared as `mcpServers` in settings. Precedence, low → high:

| layer | path | override |
| --- | --- | --- |
| system defaults | `/Library/Application Support/GeminiCli/system-defaults.json` | `GEMINI_CLI_SYSTEM_DEFAULTS_PATH` |
| user | `$GEMINI_CLI_HOME/.gemini/settings.json` | (via `GEMINI_CLI_HOME`) |
| project | `<cwd>/.gemini/settings.json` | — |
| **system settings** | `/Library/Application Support/GeminiCli/settings.json` | `GEMINI_CLI_SYSTEM_SETTINGS_PATH` |

**There is no `--settings` CLI flag** — the Claude Code fileless move has no direct analogue.

Stdio server schema:

```json
{"mcpServers":{"marion":{"command":"...","args":["..."],"cwd":"...","env":{},"trust":true,"timeout":30000,"description":"...","includeTools":[],"excludeTools":[]}}}
```

`url` (SSE) and `httpUrl` (streamable HTTP) with `headers` are also supported; precedence is
`httpUrl` > `url` > `command`. Tools reach the model as `mcp_<server>_<tool>` — so
**the server alias must not contain `_`**: the docs warn that the policy engine mis-parses the
fully-qualified name and **fails silently**. `marion` is safe.

**The recommended injection, verified end to end:** write marion's settings JSON to a temp file and
point `GEMINI_CLI_SYSTEM_SETTINGS_PATH` at it. Highest precedence, no project `.gemini/` directory,
no writes to `~/.gemini`, nothing the user owns is touched. Confirmed with only that env var set:
the MCP tool was discovered, called and executed.

Settings string values interpolate `$VAR`, `${VAR}` and `${VAR:-default}` from the environment,
which is how a per-node token can reach the child without being embedded in the file.

`gemini mcp add <name> <commandOrUrl> [args...]` exists (`-s user|project`, `-t stdio|sse|http`,
`-e KEY=val`, `-H hdr`, `--timeout`, `--trust`, `--include-tools`, `--exclude-tools`) alongside
`mcp remove|list|enable|disable`. **`gemini mcp list` connects to each server** and prints e.g.
`OK marion: ... (stdio) - Connected` — a zero-cost health check, the Gemini analogue of S8's
free `codex login status`.

## The silent-failure trap: no `trust: true` means the tools are simply not there

This is the headline for adapter work, and it belongs to §12's recurring theme of **flags whose
omission produces no error anywhere**. In headless mode with the default approval mode and no `-y`,
an MCP server **without `trust: true` has its tools omitted from the request body entirely**.

Identical prompts, measured against the fake endpoint:

| configuration | tool declared in request body | outcome |
| --- | --- | --- |
| `trust: true`, no `-y` | **YES** — 1 occurrence, 39.7 KB body | tool called, output `MARION_REPORT_OK` |
| no trust, no `-y` | **NO** — 0 occurrences, 39.3 KB body | no `tool_use` events; the model just answered |
| no trust, `-y` | **YES** — 52.9 KB body, plus all core tools | tool called |

So without `trust: true` **or** `--yolo`, marion's MCP tools are **invisible to the model**: no
prompt, no warning, no error, and a run that completes successfully having done nothing. This is
the same failure shape as S10's mis-shaped hook registration — a correct-looking run is not evidence
the tool was reachable.

Adjacent knobs: `tools.allowed`, `tools.confirmationRequired`, `tools.exclude`, `mcp.allowed` /
`mcp.excluded`, `--allowed-mcp-server-names`, and `--allowed-tools` (deprecated in favour of the
Policy Engine — `--policy` / `--admin-policy`, `docs/reference/policy-engine.md`). An admin can veto
with `security.disableYoloMode: true`, which disables `--yolo` **even when it is passed** — so a
launcher must not treat `-y` as guaranteed.

**Folder trust is a second gate.** A fresh sandbox directory needs `GEMINI_CLI_TRUST_WORKSPACE=true`
or `--skip-trust`, else the trust check can block. `GEMINI_CLI_TRUSTED_FOLDERS_PATH` relocates
`trustedFolders.json` independently of `GEMINI_CLI_HOME`. **Confirms §6.4.**

## Headless invocation

- `-p, --prompt` runs non-interactive with the given prompt; if stdin also has content, the prompt is
  **appended** to it.
- `-y, --yolo` auto-accepts all actions (default false).
- `--approval-mode` ∈ `default | auto_edit | yolo | plan`. Note `general.defaultApprovalMode`
  **cannot** be set to `yolo` — the docs state YOLO mode "can only be enabled via command line".

Verified argv:

```sh
gemini -m gemini-2.5-flash --output-format stream-json -p "<prompt>"
```

The prompt is an **argument to `-p`**. A bare positional query launches **interactive** unless
stdin/stdout is piped.

**Gemini does not refuse a non-tty stdin** — the direct contrast with S11, where `claude -p` exits 1
on a pty stdin and demands a prompt. Piping works, and stdin is **prepended as context**:
`echo "PIPED CONTEXT LINE" | gemini -p "call the report tool"` produced user content
`"PIPED CONTEXT LINE\n\n\ncall the report tool"`. Headless mode also auto-triggers on a non-TTY.

`docs/cli/headless.md` documents exit codes **0** success, **1** general/API error, **42** input
error, **53** turn limit. **But an auth failure returned exit 0 with a JSON error body** — so a
launcher must parse the JSON and must not trust the exit code alone.

## Output formats

`-o/--output-format` ∈ `text | json | stream-json` (bundle enum `OutputFormat.TEXT/JSON/STREAM_JSON`).

`json` emits one object: `{session_id, response, stats{models{<modelVersion>{api{totalRequests,
totalErrors, totalLatencyMs}, tokens{input, prompt, candidates, total, cached, thoughts, tool},
roles{…}}}, tools{…}, files{…}}}`. On failure: `{session_id, error:{type, message, code}}`.

`stream-json` is NDJSON with event types `init | message | tool_use | tool_result | error | result`.
Captured verbatim (ids and timestamps redacted per house style):

```json
{"type":"init","timestamp":"<TS>","session_id":"<UUID-1>","model":"gemini-2.5-flash"}
{"type":"message","timestamp":"<TS>","role":"user","content":"call the report tool"}
{"type":"tool_use","timestamp":"<TS>","tool_name":"mcp_marion_report","tool_id":"mcp_marion_report__mcp_marion_report_<n>_0","parameters":{"text":"hi"}}
{"type":"tool_result","timestamp":"<TS>","tool_id":"<TOOL-ID-1>","status":"success","output":"MARION_REPORT_OK"}
{"type":"message","timestamp":"<TS>","role":"assistant","content":"DONE_AFTER_TOOL","delta":true}
{"type":"result","timestamp":"<TS>","status":"success","stats":{"total_tokens":16,"input_tokens":10,"output_tokens":6,"cached":0,"input":10,"duration_ms":47,"tool_calls":1,"models":{"<REDACTED-machine-specific>":{}}}}
```

**CAVEAT:** warnings interleave on stderr/stdout — `Warning: Basic terminal detected...`,
`[STARTUP] Phase ...` — so a reader must filter to lines beginning with `{`. This is a framing
hazard of the same family S11 recorded for a pty'd stdout: the protocol is intact, the read
boundaries are not free.

## Model selection, and two canned-server gotchas

Model comes from `-m/--model`, `GEMINI_MODEL`, or `model.name` in settings. Aliases: `auto`, `pro`,
`flash`, `flash-lite`. Ids seen in the bundle: `gemini-2.5-pro`, `gemini-2.5-flash`,
`gemini-3.5-flash`, `gemini-3-flash`, `gemini-3.1-flash-lite`, `gemini-3-pro-preview`,
`gemini-3.1-pro-preview`, `gemma-4-31b-it`.

Base-URL overrides: `GOOGLE_GEMINI_BASE_URL` for `gemini-api-key`, `GOOGLE_VERTEX_BASE_URL` for
`vertex-ai`, `CODE_ASSIST_ENDPOINT` for the OAuth/Code-Assist path. **All must be HTTPS unless they
point at `localhost` / `127.0.0.1` / `[::1]`** — which is exactly marion's case, so a loopback
proxy needs no TLS.

**The wire shape is Gemini (google-genai SDK), not OpenAI.** Captured against the fake server:

```
POST /v1beta/models/gemini-3.5-flash:streamGenerateContent?alt=sse
x-goog-api-key: <GEMINI_API_KEY>
x-goog-api-client: google-genai-sdk/1.30.0 gl-node/v26.5.1
User-Agent: GeminiCLI-tui/0.53.0/gemini-2.5-flash (darwin; arm64; terminal)

{"contents":[{"parts":[{"text":"<session_context>…"},{"text":"say hi"}],"role":"user"},…],"systemInstruction":{…},"tools":[…]}
```

A canned server must answer `:streamGenerateContent` with SSE frames
`data: {"candidates":[{"content":{"role":"model","parts":[…]},"finishReason":"STOP","index":0}],"usageMetadata":{…},"modelVersion":"fake-1"}`,
and `:generateContent` with plain JSON. Tool calls come back as
`{"functionCall":{"name":"mcp_marion_report","args":{…}}}`; results are re-sent as
`{"functionResponse":…}`.

Two gotchas, each of which cost a run:

- **(a) Model routing.** With no `-m` (model `auto`) the CLI first makes a **classifier call** to
  `gemini-3.1-flash-lite` over **non-streaming `:generateContent`**, expecting a structured routing
  verdict. A naive canned reply made it retry 5× and hang. **Always pass an explicit `-m`.** It can
  also be disabled via `general.plan.modelRouting` / the experimental settings.
- **(b) The model id in the URL is not the one you asked for.** Even with `-m gemini-2.5-flash`, the
  request path was **`gemini-3.5-flash`** — an internal remap. **A fake server must match on
  substrings, not exact model ids.**

## Background processes and telemetry

**No orphans.** After every headless run, `ps aux | grep -E 'gemini|mcp.py'` was empty; the stdio MCP
child dies with its parent. This is the clean contrast to S7/§6.4's codex plugin-clone `git fetch`,
which outlives `codex exec` and reparents to pid 1.

- **Auto-update:** `general.enableAutoUpdate` and `general.enableAutoUpdateNotification` both default
  **true**. `checkForUpdates()` lives only in the `interactiveCli-*.js` chunks, so it does not run on
  the `-p` path — disable both anyway.
- **Usage telemetry:** `privacy.usageStatisticsEnabled` defaults **true**, ships to Clearcut at
  `CLEARCUT_URL = "https://play.googleapis.com/log?format=json&hasfast=true"`, buffered with
  `FLUSH_INTERVAL_MS = 60000`, and calls `systeminformation.graphics()` for GPU info. Set it
  **false**.
- **OTel telemetry** is separate and off by default: `telemetry.{enabled, traces, target,
  otlpEndpoint, otlpProtocol, logPrompts, outfile, useCollector}`, overridable by
  `GEMINI_TELEMETRY_ENABLED`, `GEMINI_TELEMETRY_TARGET`, `GEMINI_TELEMETRY_OTLP_ENDPOINT`,
  `GEMINI_TELEMETRY_TRACES_ENABLED`.
- **Keep off:** `ide.enabled` (default false; `GEMINI_CLI_IDE_SERVER_PORT` /
  `GEMINI_CLI_IDE_PID`), `general.devtools` (false), `-s/--sandbox` and `GEMINI_SANDBOX`
  (`docker`/`podman`/`seatbelt` — leave unset), `experimental.voice*`, the bundled
  `chrome-devtools-mcp.mjs`, and `node-pty` (the interactive shell tool).
- **Session artifacts** land inside the sandbox home: `.gemini/projects.json`,
  `.gemini/tmp/<project>/chats/session-*.jsonl`, `.gemini/history/`. Retention knobs:
  `general.sessionRetention.{enabled, maxAge, maxCount, minRetention}`.

## The adapter's target invocation

Recorded verbatim as the shape a marion Gemini launcher should compile to:

```sh
GEMINI_CLI_HOME=$SANDBOX \
GEMINI_CLI_SYSTEM_SETTINGS_PATH=$SANDBOX/marion-settings.json \
GEMINI_CLI_TRUST_WORKSPACE=true \
GEMINI_FORCE_FILE_STORAGE=true \
GEMINI_API_KEY=... GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:$FAKE_PORT \
gemini -m gemini-2.5-flash --output-format stream-json -p "$PROMPT"
```

with `marion-settings.json`:

```json
{"mcpServers":{"marion":{"command":"…","args":["…"],"env":{},"trust":true}},"security":{"auth":{"selectedType":"gemini-api-key"}},"privacy":{"usageStatisticsEnabled":false},"general":{"enableAutoUpdate":false,"enableAutoUpdateNotification":false}}
```

Every element of that line is load-bearing: drop `trust`, the tools vanish silently; drop
`selectedType`, the run dies with `Invalid auth method selected.`; drop
`GEMINI_CLI_TRUST_WORKSPACE`, the trust check can block; drop `-m`, the router hangs.

## Reproducing

No probe script is committed. The measurements above were made with the installed binary and a
local fake endpoint; the load-bearing commands were:

```sh
gemini --version                                        # → 0.53.0
security find-generic-password -s "gemini-cli-oauth"    # → item could not be found
GEMINI_CLI_HOME=$SANDBOX gemini -m gemini-2.5-flash --output-format stream-json -p "<prompt>"
echo "PIPED CONTEXT LINE" | gemini -p "call the report tool"
gemini mcp list                                         # zero-cost MCP health check
```

Bundle-source claims are readable straight out of the installed package —
`/opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/gemini.js` and the docs shipped beside it
at `bundle/docs/` (`cli/enterprise.md`, `cli/headless.md`, `reference/policy-engine.md`).
**Nothing here requires network access to Google, and nothing here costs money.**

## Redaction

- Session ids → `<UUID-1>`, tool-call ids → `<TOOL-ID-1>`, timestamps → `<TS>`, **numbered by first
  appearance** where correlation matters (following S9 and S10 rather than S6). The `tool_id` in the
  `tool_use` frame is left in its raw generated form because its *shape* —
  `mcp_marion_report__mcp_marion_report_<n>_0` — is the evidence.
- The `result` frame's per-model `stats.models` map is **reduced** to
  `"<REDACTED-machine-specific>"`; the key is kept so the shape still reads.
- No secret, token or credential value appears in this file. `~/.gemini/` was inspected for
  **filenames, sizes and modes only**, and the Keychain for **item existence only**.

## Still open after this

- **Whether OAuth (`oauth-personal`) credentials copied via `GEMINI_CLI_HOME` refresh correctly in a
  child is UNVERIFIED.** Not testable without a live token refresh against Google's endpoint. The
  code path is plain file/keychain reads, so it *should*, but nothing here measures it. **This is
  the Gemini analogue of the refresh-token-rotation risk §11 item 3(a) already records for Codex**,
  and it is why S12 does not close item 3.
- **Whether this machine's `~/.gemini/oauth_creds.json` belongs to the CLI or to Antigravity** is
  unresolved — both write into that directory.
- **The exact trigger conditions for the `gemini-2.5-flash` → `gemini-3.5-flash` URL remap** are
  unknown; only the fact of the remap was observed.
- **Whether Clearcut telemetry fires on the headless path** is unmeasured. Only the model endpoint
  was instrumented; there was no outbound-network observation harness.
- **The whole measurement ran on `GEMINI_API_KEY` against a fake endpoint.** A real `oauth-personal`
  child in an isolated `GEMINI_CLI_HOME` was **never exercised end to end**, so the COPYABLE verdict
  rests on the storage mechanism plus the isolation measurement, not on an executed subscription
  login.
- One machine, one CLI version, one account, one run per configuration.
