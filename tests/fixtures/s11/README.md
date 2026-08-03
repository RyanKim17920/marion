# S11 — S1's interrupt protocol over a real pty

Run 2026-08-03 against **Claude Code 2.1.220** (`claude --version` → `2.1.220 (Claude Code)`),
macOS (darwin 25.5.0, arm64), Python 3.14.6, harness `s11-pty_interrupt.py/1`
(`spikes/s11/`). **No model was called, no API key was used, no paid tokens** —
`ANTHROPIC_BASE_URL` points at a canned Anthropic-Messages provider on `127.0.0.1`
(`spikes/s11/canned_provider.py`) and `ANTHROPIC_API_KEY` is empty. Cost **$0.00**;
every `result` frame in these captures carries `total_cost_usd: 0`.

Closes design doc **§11 item 1**, and answers part of **§11 item 20**.

## ANSWER

**Yes — the interrupt protocol was captured over a real pty, and the *protocol* is
unchanged. But `isatty` does change three observable things, one of which is a genuine
parser hazard, and one of which is a hard constraint on how marion may launch a headless
root.**

1. **Same frames, same order.** `pipes` and `pty-out` produce an **identical** collapsed
   frame-kind sequence — 38 kinds long, `collapsed_sequence_identical: true`,
   `first_divergence: null` (`compare.json`). Same `distinct_frame_kinds` set. The
   interrupt semantics are identical in both: `control_response` `{"still_queued":[]}`
   — that frame is **byte-identical** across all three completing transports,
   then `result` with `is_error: true`, `subtype: "error_during_execution"`,
   `terminal_reason: "aborted_streaming"`, then a follow-up turn that succeeds
   (`terminal_reason: "completed"`, `result: "OK-AFTER-INTERRUPT"`, exit **0**). That is
   S1's recorded behaviour, reproduced over a pty.
2. **Different boundaries.** The bytes arrive in very different chunks (table below).
   Nothing is lost or reordered, but a parser that assumed a read is a frame would break.
3. **`-p` refuses a pty *stdin*.** Both `pty-in` and `pty-all` die with
   `Error: Input must be provided either through stdin or as a prompt argument when using
   --print` and **exit 1**, having emitted only the `SessionStart` hook frames. This is
   about `isatty(stdin)`, not stdout: `pty-in` gives the child a pty stdin and pipe
   stdout and fails identically to `pty-all`.

### The boundary difference, measured

| | `pipes` | `pty-out` |
| --- | --- | --- |
| stdout bytes | 130,915 | 131,449 |
| `read()` calls | 139 | **230** |
| largest single read | **46,515** | **1,024** |
| reads ≥ 4096 B | 5 | **0** |
| reads returning no complete frame | **0** | **92** (40%) |
| JSON frames parsed | 142 | 143 |
| lines ending `\r\n` | 0 | **143** |
| lines ending `\n` only | 142 | 0 |

Two mechanisms, both in the kernel's tty layer rather than in the CLI:

- **The 1024-byte ceiling.** macOS's pty output queue caps what one `read()` on the master
  can return. The ~48 kB `initialize` reply arrives on a pipe as **one 46,515-byte read**
  and on a pty as **~47 reads of 1024 bytes**. 92 of the 230 pty reads contain no line
  terminator at all — they are mid-frame fragments. On pipes that happened **zero** times.
  **A reader must buffer across reads and split on `\n`.** Reading a fixed-size chunk and
  parsing it is fine on a pipe here and is broken on a pty.
- **`ONLCR`.** Every line on the pty ends `\r\n`. This is the line discipline's LF→CRLF
  translation, not the CLI: see "attribution" below.

### The `\r`: attributed to the kernel, not the CLI — measured

`pty-out-raw/` is a fourth capture: `spikes/s11/pty_interrupt.py --raw-output` clears
`OPOST` on the pty slave (which disables `ONLCR`) and is otherwise byte-for-byte the same
harness, same argv, same script. Result:

| | `pty-out` | `pty-out-raw` (OPOST off) |
| --- | --- | --- |
| lines ending `\r\n` | 143 | **0** |
| lines ending `\n` only | 0 | **141** |
| `read()` calls | 230 | **230** |
| largest single read | 1,024 | **1,024** |

**The `\r` is the line discipline's `ONLCR`, not the CLI.** Claude Code writes the same
LF-terminated bytes to a pipe and to a pty; the kernel adds the `\r` on the way out of a
terminal. And the two effects are independent: turning off `ONLCR` removes every `\r` and
changes the read-chunk count **not at all** — the 1024-byte ceiling is a separate
mechanism, and it is the one that actually reshapes the framing.

The consequence for marion stands regardless: **a stream-json reader
on a pty must tolerate a trailing `\r`.** Python's `json.loads` and `serde_json::from_str`
both accept trailing whitespace, so this is survivable by accident — but a reader that
compares raw line bytes, or splits on `b"}\n"`, or hashes lines, will see different bytes
on a pty than S1's fixture shows.

### Frame *bytes*, not just frame kinds — measured

`spikes/s11/frame_equality.py` compares the frames themselves rather than their kinds.
Normalising only the four things that legitimately differ between two runs of the same
script — per-run UUIDs, wall-clock `timestamp` strings, wall-clock durations, and how far
the canned counting stream got before the interrupt landed — and excluding
`content_block_delta` frames (whose *count* is timing, not transport):

```
normalised_non_delta_frames: pipes 36, pty-out 36, pty-out-raw 36
identical_to_pipes:          pty-out true, pty-out-raw true
```

**36 of 36 non-delta frames are byte-identical across all three transports.** The only
difference that survived normalisation during development was the `timestamp` field on one
`assistant` frame, which is wall clock. So the answer to "same frames" is not merely "same
kinds in the same order" — it is the same bytes.

### The other isatty-conditional difference: colour

The stderr warning Claude Code emits about connectors is **plain text under `pipes`** and
**ANSI-coloured under `pty-out`** (`\x1b[33m…\x1b[39m`) — see the `stderr` field of the two
`summary.json` files. stderr was a *pipe* in both runs; only stdout differed. So the
colour decision is keyed on `isatty(stdout)`, and it propagates to a stream marion also
reads. **Nothing coloured reached stdout** in this capture — `non_json_stdout_count: 0`
for `pty-out`, all 143 lines parsed as JSON — so the stream-json channel itself is clean.
But the mechanism that would dirty it is demonstrably active on the pty path.

### Terminal probes: none, on any transport

`probes_seen` is **empty in all four runs**, including `pty-all` where the child owned the
pty as its controlling terminal. The harness watches for DA1, DA2, XTVERSION, CPR and
DECRQM-2026 on the output stream and answers none of them by default. **Headless
`claude -p` emits no terminal probes at all**, so the "who answers the probes" question
does not arise on this path. That is a narrower statement than §11 item 20's, which is
about the *TUI* path; this fixture says nothing about the TUI.

### Latency, and what a single run can say

§11 item 11 flags that S1's headline numbers rest on one run on one machine. Three runs per
transport cannot fix that; here is what they show (ms, `compare.json → latency_repeats`):

| | interrupt → `control_response` | interrupt → terminal `result` |
| --- | --- | --- |
| **S1 (pipes, one run, real model)** | 0.500 | 1.868 |
| S11 `pipes` ×3 | 0.775 / 0.368 / 0.958 | 6.18 / 3.29 / 8.50 |
| S11 `pty-out` ×3 | 1.231 / 1.256 / 1.670 | 9.05 / 11.00 / 13.09 |

The pipe control response reproduces S1's ~0.5 ms to within its own spread. The pty is
**consistently slower** — every pty run exceeds every pipe run on both measures — which is
what 230 reads instead of 139 would predict. Order of magnitude is unchanged: sub-2 ms for
the control response, low tens of ms to the terminal result, on both transports. **Six
runs on one machine. Do not read the pipe/pty ordering as established; read it as "not
contradicted, and mechanistically plausible".**

## What is here

| path | what it is |
| --- | --- |
| `pipes/` | S1's transport, the control: stdin/stdout/stderr all pipes |
| `pty-out/` | **the item-1 measurement**: stdout is a real pty, stdin and stderr pipes |
| `pty-out-raw/` | same, with `OPOST` cleared — attributes the `\r` to `ONLCR` |
| `pty-in/` | stdin is a pty, stdout/stderr pipes — isolates which fd causes the refusal |
| `pty-all/` | stdin+stdout+stderr one pty, child owns it as controlling terminal |
| `compare.json` | machine-generated comparison (`spikes/s11/compare.py`) |

Each run directory holds:

- `stdout.jsonl` — one line per JSON frame, in arrival order, with `t_rel` (monotonic
  seconds since spawn) and `chunk` (the index of the `read()` that completed it). **The
  `chunk` column is the framing evidence**: `pipes` frames have near-consecutive chunk
  indices, `pty-out` frames skip indices wherever a read returned only a fragment.
- `chunks.jsonl` — every `read()`/`write()` with timestamp, direction tag and byte count.
  `O` = child stdout, `I` = harness stdin write, `E` = child stderr.
- `summary.json` — argv, env overrides (names only, never values), the protocol events
  with latencies, exit status, stderr, probes seen, and the framing totals.

`raw.bin` (the unredacted byte stream) is produced by the harness and deliberately **not
committed**.

## Reproducing

```sh
sh spikes/s11/run.sh /tmp/s11 8111 3            # all 5 captures + compare.json
for t in pipes pty-out pty-out-raw pty-in pty-all; do
  python3 spikes/s11/redact.py /tmp/s11/run-$t tests/fixtures/s11/$t /tmp/s11
done
cp /tmp/s11/compare.json tests/fixtures/s11/compare.json
python3 spikes/s11/frame_equality.py            # the byte-equality claim above
```

Needs real `claude` (2.1.220) on `PATH` (or `S11_CLAUDE=<path>`) and Python 3. `run.sh`
starts a fresh canned provider per run — the provider's turn counter is per-process and
turn 1 is the long interruptible stream, so a reused provider hands the wrong turn to the
next run. No network egress; nothing is left running.

The argv is **S1's, verbatim** from `tests/fixtures/s1/summary.json`:
`-p --output-format stream-json --input-format stream-json --include-partial-messages
--verbose --model haiku --allowed-tools ""`. The stdin script is S1's, verbatim from
`tests/fixtures/s1/stdin.jsonl`. **The one deviation**: S1's script fired on wall-clock
offsets; S11 fires on events (interrupt goes 3.0 s after the first content delta) so the
interrupt reliably lands mid-stream regardless of how the canned provider paces.

## Redaction

`spikes/s11/redact.py`, no hand editing. Home paths → `<HOME>`; the per-run scratch dir →
`<SCRATCH>` (with and without macOS's `/private` prefix); UUIDs → `<UUID-N>` numbered by
first appearance (S9's scheme, kept because arrival-order correlation is evidence here).
**Reduced, not merely scrubbed**, per S9: the `initialize` reply's `commands`, `agents`,
`models`, `available_output_styles`, `account` and `pid`, the `system/init` frame's
`slash_commands`/`skills`/`agents`/`plugins`/`mcp_servers`/`memory_paths`/`tools`/`cwd`/
`model` and friends, and every hook body → `<REDACTED-machine-specific>` /
`<REDACTED_HOOK_OUTPUT>`. Keys are kept so the shape still reads. That is also why
`stdout.jsonl` is 39 kB rather than 130 kB: the `initialize` reply alone is ~48 kB of
machine-specific catalogue (S9 recorded the same thing).

No credential appears anywhere: `summary.json` records env variable **names** only.
`ANTHROPIC_AUTH_TOKEN` was set to the literal non-credential string the harness hardcodes.

## What this says about `tests/fixtures/s2/ptyhost.py` (§11 item 20)

`s2/ptyhost.py` **could not have taken this measurement**, and was not modified — S11 is a
separate host. It is a *TUI* capture host: one pty for everything, a wall-clock script of
keystrokes, a raw byte log, no protocol parsing, no separate stdin channel, and no reaction
to anything the child emits. Three of those are disqualifying here:

- it gives the child a **pty stdin**, which is exactly the configuration `-p` refuses
  (`pty-in`/`pty-all` above) — so it cannot drive a headless stream-json session at all;
- it has **no event loop over frames**, so it cannot send an interrupt "3 s after the first
  delta", only "at t=7.5 s", which is the timing dependence S11 removes;
- it **never reads its own log**, so it cannot answer probes or measure a round trip.

It is not *broken* — it did what S2 needed. It is the wrong shape for a protocol
measurement. `spikes/s11/pty_interrupt.py` is the replacement for that purpose: it keeps
`ptyhost.py`'s length-prefixed `raw.bin` record format (`tag + f64 + u32 + payload`, so
`s2/extract.py` still applies) and adds the frame parser, the event-driven script, the four
fd topologies, and probe detection with optional answering.

**What §11 item 20 still wants and S11 does not give it**: a real terminal emulator with
real rendering. S11 answers "does a pty change the *framing* of the control protocol"
(no, apart from boundaries) and "does headless `claude -p` probe the terminal" (no). It
does not answer item 9's `ESC[6n` stall or the keystroke-injection submit check, both of
which are TUI questions.

## Still unmeasured

- **`--include-partial-messages` was on** (S1's argv). Whether the boundary numbers hold
  without it is untested; fewer, larger frames would change the read-chunk ratio.
- **One machine, one OS, one CLI version.** The 1024-byte pty ceiling in particular is a
  macOS number. Linux's pty buffer is larger, so the *magnitude* of the boundary
  difference will differ there even though the direction should not.
- **Nothing here exercises `--permission-prompt-tool stdio`**, so like S1 these captures
  contain zero inbound `control_request` frames. That is S9's territory, not S11's.
- **The TUI path over a pty** — probes, rendering, `ESC[6n` — is untouched (see above).
