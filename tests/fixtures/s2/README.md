# S2 — what a real TUI writes to a pty

Recorded **2026-07-31** against **Claude Code 2.1.220** and **Codex CLI 0.145.0 and 0.146.0**. This
directory predates the per-spike README convention; everything below is quoted from the provenance
row for `s2` in [`../REVIEW.md`](../REVIEW.md), from `s2/NOTES.txt`, or read off the files.

## What the captures are

`REVIEW.md`: *"pty captures of Claude Code 2.1.220 and Codex CLI 0.145.0/0.146.0 booting and running
local slash commands (`/help`, `/status`, `/diff`, `/exit`) across resizes. Five captures, each as
`.raw.bin` + asciicast v3 `.cast`."*

| Capture | Harness |
|---|---|
| `claude-2.1.220-boot-exit` | Claude Code 2.1.220 |
| `claude-2.1.220-boot-help-status-resize` | Claude Code 2.1.220 |
| `codex-cli-0.145.0-boot-status-help-diff-resize` | Codex CLI 0.145.0 — the alt-screen / `/diff` capture |
| `codex-cli-0.146.0-boot-status-help-resize` | Codex CLI 0.146.0 |
| `codex-cli-0.146.0-14row-heavy-history-insert` | Codex CLI 0.146.0 |

The `.raw.bin` is the pty master's bytes verbatim; the `.cast` is the asciicast v3 rendering of the
same run. **No model was called** — `REVIEW.md` records the provider as *"**None** — no model calls;
only local slash commands were driven"*. Alongside them: `ptyhost.py` (the recorder), `extract.py`
(raw → cast), and `analyze.py` / `scrollattr.py` (the analyses), with `NOTES.txt` as the running
record.

## Which tests read it

Two, both in `crates/marion-supervisor/src/pty/tests.rs`:

- `the_cast_header_and_record_shape_match_the_committed_captures` reads
  `codex-cli-0.146.0-boot-status-help-resize.cast` and checks marion's own `CastWriter` against it —
  on *decoded* values, not bytes, so the assertion does not pin Python's `json.dumps` escaping.
- `the_reader_carries_an_incomplete_utf8_sequence_across_reads` reads
  `claude-2.1.220-boot-exit.raw.bin` and re-chunks it at 1, 2, 3, 7, 64 and 1024 bytes. Its reason is
  this corpus: *"`tests/fixtures/s2/NOTES.txt` records 23 U+FFFD in 9 regions across three committed
  `.cast` files, caused by `extract.py` calling `decode(..., "replace")` per pty read chunk. The
  captures themselves are valid UTF-8 end to end."*

Several constants are derived from these captures rather than chosen —
`crates/marion-supervisor/src/pty.rs` cites them for the cast shape, the `"100x24"` size field, the
read-buffer size and the reader's idle sleep, and `crates/marion-supervisor/src/attach.rs` cites
`claude-2.1.220-boot-exit.raw.bin` for the escape sequences it names. `marion-term` derived both
columns of the vt100-vs-alacritty scrollback comparison from these files.

## Redaction — read this before re-recording

`s2` is the corpus with the **worst** redaction history in the repo, and the full account is in
[`../REVIEW.md`](../REVIEW.md) and `NOTES.txt`. Three things to know:

1. **It was not recorded under an isolated `HOME`** (*"S4 did this; **S1 and S2 did not** … why S2
   still renders the operator's plugin catalogue"*). The `/help` and boot panels show the operator's
   real plugin-scoped command names and MCP state. `REVIEW.md`: *"**Any re-record of S2 must use a
   scratch `HOME`.**"*
2. Redaction here is **equal-length only** — byte offsets are load-bearing for the analyses, so
   emails became per-site placeholders and the username became `example`, nothing more.
3. An earlier claim that the DECSTBM histogram was unchanged by that pass **was retracted**: nine
   splices had landed inside CSI sequences and forged scroll-region commands. Repaired and
   re-derived 2026-08-01.

Accepted residue, both stated in `REVIEW.md`: ~4.4 KB of an unrelated project's file bodies inside
the 0.145.0 `/diff` pager span (generated `.serena/*` boilerplate, no identity content), kept because
that span is exactly what fixtures the transient alt-screen entry; and the plugin/skill catalogue
above. **Do not add a capture here without adding its row to `REVIEW.md`.**
