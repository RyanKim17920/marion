# S14 — the harness-native mapping for `read`, measured on all four harnesses

Run 2026-08-05 on macOS (darwin 25.5.0, arm64) against the installed binaries — **claude 2.1.222,
codex-cli 0.146.0, gemini 0.53.0, opencode 1.17.3** — each driven against the workspace's own
**canned provider** (`crates/marion-provider`, `target/debug/canned`) on `127.0.0.1`.
**No vendor endpoint was contacted, no model was called, no paid token was spent, no real
credential was read. Total spend: $0.00.**

Answers the question design doc §3.1 makes a precondition of shipping a verb: **`tools:` currently
has a one-word vocabulary (`write`) because the project's rule is that a verb ships only with a
*measured* harness-native mapping.** This fixture measures the mapping for `read` so that the
wiring change can cite it. **The wiring is deliberately not in this change.**

## ANSWER, in one line per harness

| harness | `read` maps to | availability flag | unknown name | `read` by default |
| --- | --- | --- | --- | --- |
| claude 2.1.222 | **`Read`** | `--tools` — **gates** | **SILENTLY IGNORED** | yes by CLI default, **no** under marion's hardcoded `--tools ""` |
| codex 0.146.0 | **nothing — there is no read tool** | none (`--tools` is a hard argv error) | n/a | n/a |
| gemini 0.53.0 | **`read_file`** | none measured that gates it | **SILENTLY IGNORED** by `--allowed-tools` | **yes** — granting `read` is a no-op |
| opencode 1.17.3 | **`read`** | `OPENCODE_PERMISSION` — **`deny` gates** | **SILENTLY IGNORED** | **yes** — granting `read` is a no-op |

**Three of the four silently ignore an unknown tool name — claude, gemini and opencode — each on
its own configuration surface: `--tools`, `--allowed-tools`, and `OPENCODE_PERMISSION`
respectively.** The fourth, codex, **has no per-tool flag at all**; it errors because `--tools`
itself is unsupported argv, not because it validates names. So codex is not a fourth data point on
the same axis, and it is not evidence that anything validates: there is no surface there to ignore
a name *on*.

So §3.1's rule — a verb ships only with a measured mapping — is not a stylistic preference on three
of four harnesses: it is the only thing standing between a typo and a child that runs green with no
tool.

> **This paragraph read "Two of the four" until 2026-08-07, contradicting the table directly above
> it**, which has marked `SILENTLY IGNORED` in three rows since the fixture was written. The
> sentence following it ("only codex errors") was right the whole time, which is what made the
> wrong count survive: the shape of the claim was correct and only the number was not. The count
> had already propagated to `marion-core/src/agent_type.rs`, which said three and was therefore
> right about the number while still conflating the three surfaces into claude's `--tools`.

## The critical negative, verbatim

**claude.** Same argv, same prompt, one word different, and nothing anywhere reports a problem:

```
--tools ""              → body.tools []                     exit 0
--tools Read            → body.tools ["Read"]               exit 0
--tools NotATool        → body.tools []                     exit 0   ← empty stderr
--tools Read,NotATool   → body.tools ["Read"]               exit 0   ← bad name dropped, good one kept
--tools read            → body.tools []                     exit 0   ← marion's OWN word, unmapped
```

**The last line is the whole reason this fixture exists.** `read` is marion's vocabulary; `Read` is
the harness's. Shipping the verb without the mapping — passing marion's word straight through —
produces a child with **no tools at all**, exit 0, empty stderr, and a `system/init` frame that
agrees. It is indistinguishable from a healthy run, and indistinguishable from the bogus `NotATool`
run above. That is the accept-and-ignore shape §3.1's availability axis was built to eliminate, and
it is measured here rather than argued.

The `system/init` frame agrees with the request body in every case (`"tools": []` for the bogus
run), so there is no second surface that would have caught it either. `--help` on 2.1.222 reads
*"Specify the list of available tools from the built-in set. Use `""` to disable all tools,
`"default"` to use all tools, or specify tool names (e.g. `"Bash,Edit,Read"`)."*

**gemini.** `--allowed-tools read_file` and `--allowed-tools NotATool` produce **byte-identical
declarations** — the same 8 functions, exit 0 — differing from the run with no flag at all only by
a deprecation warning on stderr. The flag neither gates nor validates.

**opencode.** `OPENCODE_PERMISSION='{"NotATool":"deny"}'` leaves all 10 tools declared and the run
unaffected; `'{"read":"deny"}'` removes `read` from the schema (10 → 9). This is S13's
`additionalProperties` observation turned into a measurement.

**codex** is the one loud harness, and only by accident of not having the flag:

```
$ codex exec --tools Read …
error: unexpected argument '--tools' found
  tip: to pass '--tools' as a value, use '-- --tools'
exit 2
```

## What each harness actually declares

**claude 2.1.222** with `--tools` omitted, and identically with `--tools default` — 11 built-ins:

```
Agent, Bash, Edit, Read, ReportFindings, ScheduleWakeup, Skill, ToolSearch, Workflow,
DeferredToolPlaceholder, Write
```

So `read`→`Read` is now **measured** rather than asserted; design line ~168 pinned it to 2.1.220
with no fixture behind it, and this confirms it on the 2.1.222 the machine actually has.

**codex 0.146.0**, `code_mode_tool_names`, with no MCP server declared — **identical under
`--sandbox read-only` and `--sandbox workspace-write`**:

```
apply_patch, create_goal, exec_command, get_goal, update_goal, update_plan, view_image, write_stdin
```

Two things follow. **There is no read tool**: reading a file on codex is `exec_command`, i.e. the
shell. And **`--sandbox` is not an availability axis** — the declaration does not move between
modes. It constrains at *call* time, which the read-only run's own stderr shows:
`error=patch rejected: writing is blocked by read-only sandbox`. That is a sharper statement than
§3.1's "the harness's coarsest equivalent", and it means `sandbox:read-only` in
`TaskContract.allowed_tools` records an execution constraint, not a tool list.

(S6's committed capture lists twelve names for the same version. The four extra —
`list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource`, `mcp__marion__report` —
are the MCP surface, which this probe deliberately did not declare. The two captures agree.)

**gemini 0.53.0**, `functionDeclarations`, default approval mode, no MCP server:

```
update_topic, list_directory, read_file, grep_search, glob, google_web_search, enter_plan_mode,
invoke_agent
```

This reproduces §11 item 24's list independently (item 24's ten included the two `mcp_marion_*`
tools this probe has no server for) and confirms **`read_file` is present by default**.

**opencode 1.17.3**, default `permission`:

```
bash, edit, glob, grep, read, skill, task, todowrite, webfetch, write
```

opencode's own vocabulary is marion's vocabulary, word for word, for `read`, `edit` and `bash`.

## Two findings this probe produced that it was not looking for

1. **The comma separator is now measured for the multi-name case.**
   `crates/marion-harness/src/claude_code.rs` says of `--tools`: *"The multi-name case is presently
   unreachable rather than measured … whoever widens that vocabulary owes this separator a
   measurement."* `--tools "Read,Bash"` declares **both** (`claude-readbash.declaration.json`).
   That debt is paid; widening the vocabulary no longer owes it.
2. **`--approval-mode plan` on gemini *adds* write tools rather than removing them.** Against the
   8-tool default it declares 10, gaining `replace` and `write_file` and swapping
   `enter_plan_mode` for `exit_plan_mode`. §11 item 24 treats gemini's approval mode as its
   availability lever; `plan` moves that lever in the direction opposite to its name. Not chased
   here — recorded so the next person does not assume.

## Files

- `probe-claude.sh`, `probe-codex.sh`, `probe-gemini.sh`, `probe-opencode.sh` — the probes,
  verbatim as run. Argv and environment are copied from the four adapters' `compile_*` functions so
  the probe measures the shape marion actually compiles.
- `declarations.json` — every run's outcome in one file: declared names, exit code, stderr.
- `<harness>-<run>.declaration.json` — per run, the request **envelope** (seq, method, path, wire,
  headers with credentials redacted) plus the **verbatim** tool declaration off the wire. For codex
  that is `code_mode_tool_names` plus the `additional_tools` names; for gemini the `tools` array
  with its `functionDeclarations`; for claude and opencode the `tools` array.

**Full request bodies are deliberately NOT committed.** Every harness on this machine folds the
operator's own configuration into the system prompt — `~/.claude/CLAUDE.md`, the private memory
file, `~/.agents/skills/**` — and committing that would publish the operator's setup to prove a
tool name. The declaration is the entire evidence for every claim in this file, and it is here
verbatim. Re-running the probes regenerates the full logs locally.

## Reproducing

```sh
cargo build -p marion-provider --bin canned
OUT=/tmp/s14 zsh tests/fixtures/s14/probe-claude.sh     # likewise -codex, -gemini, -opencode
```

The gemini probe needs no Google access: it uses S12's technique — a throwaway `GEMINI_API_KEY`
with `GOOGLE_GEMINI_BASE_URL` at the canned server, plus
`{"security":{"auth":{"selectedType":"gemini-api-key"}}}` in a sandbox `settings.json`. **The
assumption that gemini parses arguments before it authenticates was verified, not relied on**: the
vendor-side `IneligibleTierError` block on the personal login is never reached because no Google
endpoint is contacted. Without `selectedType` the run dies `Invalid auth method selected.` exit 41,
which is S12's finding reproduced.

Every opencode run ends **exit 124** on a 25-second bound. That is S13's *"opencode never exits on a
provider hang"* reproduced against a canned script it cannot satisfy, **not** a failed measurement:
the declaration is on the wire in the first seconds and the bound is what stops the retry loop. An
unbounded first attempt produced a 2.4 GB request log before it was stopped.

## What this does NOT prove

- **Nothing about wiring.** No `read` verb exists in `marion_core::agent_type` after this change and
  none should; this fixture is the citation a later change makes, not the change.
- **Nothing about whether the declared tool works.** A name in `body.tools` is a name the model was
  offered. §11 item 24 is the standing warning here: `--tools "Write"` alone was *necessary and not
  sufficient* — the call went to `--permission-prompt-tool stdio`, where marion has no answerer, and
  the child got item 22's dead-end message instead of a write. **The same two-axis trap applies to
  `Read`**: availability without the matching `--allowedTools` entry is not a route. This probe
  measured the availability axis only.
- **Nothing about the permission axis on the other three.** `--allowedTools` was set alongside
  `--tools` on claude but its effect was not isolated; gemini's Policy Engine (`--policy`,
  `--admin-policy`), which is what `--allowed-tools` is deprecated in favour of, was **not measured
  at all**; codex's `default_tools_approval_mode` was not re-measured (S6 has it).
- **Nothing about `edit` or `bash`.** `Bash` was declared once to settle the comma separator, and
  that is the only claim made about it. `edit` was not probed on any harness. Design line ~168's
  `edit`→`Edit` remains asserted rather than measured, exactly as `read`→`Read` was before this.
- **Nothing about MCP tools.** No probe declared an MCP server, which is why the codex and gemini
  lists here are shorter than S6's and item 24's. `--tools` does not gate MCP tools (§3.1) and this
  fixture does not re-test that.
- **Nothing about other versions.** Four binaries, four versions, one machine, one OS. `claude`'s
  behaviour is recorded against **2.1.222**, which is entry one of `PINNED_HARNESSES`, not the
  2.1.220 pin.
- **Nothing about near-misses beyond case.** `NotATool` and lowercase `read` were tried on claude;
  neither was tried on gemini or opencode, where only a plainly bogus name was used. Whether
  gemini's `read_file` or opencode's `read` are case- or separator-sensitive is unmeasured.
