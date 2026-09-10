# S30 — does gemini's system-settings layer merge `mcpServers` per key, or replace it?

Measured 2026-09-10 against **Gemini CLI 0.53.0** (`gemini --version`), macOS darwin 25.5.0,
node 26.8.2 (`/opt/homebrew/lib/node_modules/@google/gemini-cli`). **No model was reached and no
real credential was used**: the auth type is `gemini-api-key` with a fake key and a dead base URL
(`http://127.0.0.1:9/`), and every probe stops at the MCP handshake or at the first failed fetch.
The operator's real `~/.gemini` was never opened — `HOME` and `GEMINI_CLI_HOME` both point at an
isolated directory under the scratchpad.

## Why this was measured

Marion declares its MCP bridge to a gemini node through `GEMINI_CLI_SYSTEM_SETTINGS_PATH`
(`marion_harness::gemini::SPEC.live_declaration`), the **system** settings layer, which S12 showed
outranks the operator's own `~/.gemini/settings.json`. S12 measured the *precedence* but not the
*granularity*: if the system layer's `mcpServers` replaced the user layer's wholesale, a native
`marion gemini` node would see marion's bridge and none of the servers the operator configured —
the §6.4 failure. `PRODUCTION_NATIVE_FACADES` kept the gemini lane disabled until this was known.

## Result

**Per key.** Both servers appear, in the TUI and headless, and the operator's other `general.*`
settings survive marion's `general` block.

| file | what it is |
| --- | --- |
| `user-settings.json` | the operator's layer (`$GEMINI_CLI_HOME/.gemini/settings.json`): `mcpServers.pencil`, `general.vimMode: true`, auth `gemini-api-key` |
| `system-settings.json` | what `GEMINI_CLI_SYSTEM_SETTINGS_PATH` names, shaped as `settings_json_with_auth` emits it: `security.auth.selectedType`, `privacy.usageStatisticsEnabled`, `general.enableAutoUpdate*`, `mcpServers.marion` with `command`/`args`/`env`/`trust: true` |
| `stdio-mcp.js` | the stand-in server both entries launch: a ~40-line stdio MCP server answering `initialize`, `tools/list` (one tool `<name>_ping`) and `tools/call`; `argv[2]` is its name so the tool list says which entry launched it |
| `env.sh` | the environment the probes ran under: the row's live-route variables plus the isolated home and the dead endpoint |
| `tui-mcp.py` | the PTY driver (117×43, the facade E2E's `OPERATOR_SIZE`): starts `gemini`, waits for its first screen, types `/mcp`, Enter, renders both screens through `pyte`, kills and reaps the child |
| `tui-mcp.screen.txt` | **the interactive shape's answer**, rendered: first screen, then `/mcp` |
| `mcp-list.txt` | `gemini mcp list` under the same layers |
| `user-settings.collision.json`, `mcp-list.collision.txt` | the same-key probe: the operator also declares a server named `marion` (its stub named `operator-owned-marion`); which one gemini launches says which layer wins a collision |
| `stream-json.ndjson`, `stream-json.stderr.txt` | `gemini -m gemini-2.5-flash --output-format stream-json -p "say hi"` under the same layers; the headless `init` frame |

### The TUI (`tui-mcp.screen.txt`)

First screen: `Authenticated with gemini-api-key /auth`, no auth dialog (a selected type plus the
env key is enough), status bar **`2 MCP servers`**, and the composer in **`[INSERT]`** with
`Press 'Esc' for NORMAL mode` — that is the user layer's `general.vimMode: true`, alive under a
system layer that also wrote into `general`.

After `/mcp`:

```
Configured MCP servers:

🟢 pencil - Ready (1 tool)
  Tools:
  - mcp_pencil_pencil_ping

🟢 marion - Ready (1 tool)
  Tools:
  - mcp_marion_marion_ping
```

Both connected, both tool lists fetched. (`mcp_<server>_<tool>` is the S12 spelling; the tool is
`pencil_ping` so the name doubles.)

### Headless (`mcp-list.txt`)

```
✓ pencil: node …/stdio-mcp.js pencil (stdio) - Connected
✓ marion: node …/stdio-mcp.js marion (stdio) - Connected
```

### Same-key collision (`mcp-list.collision.txt`)

With the user layer also declaring `marion` → `stdio-mcp.js operator-owned-marion`, the listed
`marion` runs `stdio-mcp.js marion`: **the system layer's entry wins**, so for the node's life an
operator-owned server that happens to be called `marion` is shadowed, not merged. The operator's
other servers are untouched (`pencil` still listed).

### The `stream-json` init frame does not list MCP servers

```
{"type":"init","timestamp":"…","session_id":"dfeb512b-…","model":"gemini-2.5-flash"}
{"type":"message","timestamp":"…","role":"user","content":"say hi"}
```

On 0.53.0 the frame carries `session_id` and `model` only — no `mcp_servers` (that field is Claude
Code's and qwen's, see S25). The run then retried the dead endpoint until `timeout` ended it
(exit 124; `stream-json.stderr.txt` is the head of that retry loop). So the headless oracle for
this question is `gemini mcp list`, and the interactive oracle is `/mcp`.

## The bundle agrees (`chunk-54WRQOSV.js`, `packages/cli/src/config/settings.ts` +
`utils/deepMerge.ts`)

```js
mcpServers: { …, mergeStrategy: "shallow_merge" /* SHALLOW_MERGE */, additionalProperties: { … } }
```

```js
function mergeSettings(system, systemDefaults, user, workspace, isTrusted) {
  const safeWorkspace = isTrusted ? workspace : {};
  const schemaDefaults = getDefaultsFromSchema();
  return customDeepMerge(getMergeStrategyForPath, schemaDefaults, systemDefaults, user, safeWorkspace, system);
}
```

`mergeRecursively` applies `shallow_merge` as `target[key] = { ...obj1, ...obj2 }` — one level of
keys, later source wins per key — and, for a path with no strategy (every leaf under `general`,
`security`, `privacy`), recurses into plain objects and assigns leaves. `system` is the **last**
source, which is both S12's precedence and this fixture's collision rule.

## What this changes

`PRODUCTION_NATIVE_FACADES`'s gemini lane is `enabled: true` as of this measurement
(`crates/marion-core/src/native_facade.rs`), and `settings_json_with_auth`'s doc comment states
the merge instead of the question. The facade E2E (`tests/native_facade_e2e.rs`) now runs five
lanes. **Not** changed: the row still declares through the system layer — the only route gemini
0.53.0 offers for a per-process declaration (`gemini --help` has `--allowed-mcp-server-names` but
no `--mcp-config`; the project-level `.gemini/settings.json` would be a write into the operator's
worktree, and the workspace layer is merged *before* user and system anyway).

## Reproducing

`env.sh` hard-codes the scratchpad path this was run from; point `S30` at a copy of this directory
(with `home/.gemini/settings.json` ← `user-settings.json` and `cwd/` created) and run
`env.sh gemini mcp list`, or `uv run --with pyte python3 tui-mcp.py`. The TUI driver needs `pyte`;
the stub needs `node`.
