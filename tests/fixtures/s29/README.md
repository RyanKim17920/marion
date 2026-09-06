# S29 — where `-C` goes on a `codex exec resume`

Measured 2026-09-06 against **codex-cli 0.147.0** (`codex --version`), macOS darwin 25.5.0, with
an isolated empty `CODEX_HOME` and a bogus session id. **No model was called, no API key was
used**: every probe fails before any network request, and the failure it fails with is the fact.

## Why this was measured

The demo ran `marion resume <id> --prompt …` on a codex root. The supervisor relaunched
generation 2 and journaled it; the child exited **2** with

```
error: unexpected argument '-C' found
  tip: to pass '-C' as a value, use '-- -C'
Usage: codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]
```

because `codex::SPEC` rendered `exec resume <id> --json --skip-git-repo-check -C <cwd> <prompt>`.
`tests/restart_resume.rs` was green because it asserted the relaunch and the journal, not the
resumed child's turn.

## What is here

| file | what it is |
| --- | --- |
| `codex-exec.help.txt` | `codex exec --help`, verbatim |
| `codex-exec-resume.help.txt` | `codex exec resume --help`, verbatim |
| `codex-exec-resume.argv-probe.txt` | seven argv orderings, each with codex's first lines and exit status |

## What was found

**`-C/--cd <DIR>` is an `exec` option that the `resume` subcommand does not have.** It is on
`codex exec --help` and absent from `codex exec resume --help`. `exec`'s usage line is
`codex exec [OPTIONS] <COMMAND> [ARGS]`, so the one place it can go is **ahead of `resume`**:

| argv | result |
| --- | --- |
| `exec resume <id> --json --skip-git-repo-check -c k=v -C <dir> <prompt>` | exit 2, `unexpected argument '-C'` (the demo's argv) |
| `exec resume --json -C <dir> <id> <prompt>` | exit 2, the same |
| `exec -C <dir> resume <id> --json --skip-git-repo-check -c k=v <prompt>` | **parsed**; fails later at `Model provider 'canned' not found` (exit 1) |
| `exec -C <dir> --json --skip-git-repo-check -c k=v resume <id> <prompt>` | parsed, the same later failure |
| `exec -C <dir> --json --skip-git-repo-check resume <id> -c k=v -m gpt-5 <prompt>` | parsed, the same later failure |
| `exec -C <dir> resume <id> … --output-last-message <f> --output-schema <f> <prompt>` | parsed, the same later failure |
| `exec -C <dir> resume <id> <prompt> --json` | parsed; fails at the git-repo check, so `--json` after the prompt is accepted too |

"Parsed" means clap accepted the whole argv and codex went on to resolve configuration, which is
where the bogus `-c model_provider=canned` stops it — deliberately, so no probe reaches a network.
Every other flag the row carries (`--json`, `--skip-git-repo-check`, `-c`, `-m`,
`--output-schema`, `-o/--output-last-message`) is listed by **both** helps and was accepted on
either side of `resume <id>`.

## What changed because of it

`crates/marion-harness/src/codex.rs`'s headless row places `Arg::Flag("-C", Field::Cwd)` before
`Arg::Resume`. A fresh `exec` reads its options in any order, so the same position serves both the
first launch and the resume — one row, no resume-only branch. Pinned by
`codex::tests::a_resume_places_the_working_root_ahead_of_the_subcommand_that_does_not_take_it`
(RED on the pre-fix row with `["exec", "resume", "019a-thread", …, "-C", "/tmp/wt", …]`).

The pane row is untouched: the TUI's `codex resume` is a different grammar and still unmeasured,
so a paned resume stays refused.

`tests/restart_resume.rs` now asserts the second life's own turn (a post-relaunch request carrying
the resume prompt and the first life's marker) and its code-0 exit. With the pre-fix row it fails
in about two seconds at that turn assertion, quoting the exit; with the fixed row it passes on
0.147.0 (`~/.codex/packages/standalone/releases/0.147.0-…/bin` first on `PATH`).

## Also seen: 0.153.4

The `current` symlink auto-updated to **0.153.4** between the probe above and the E2E run (§7.7's
hazard). The seven-line probe was re-run on 0.153.4 within the hour: **identical exits and
identical first lines** (`-C` after `resume` still exits 2 with the same message; `-C` before
`resume` still parses). Not added to `PINNED_HARNESSES` here — that needs the codex cells re-run,
which is its own change; the fixture files are the 0.147.0 capture.

## Reproducing

```sh
CODEX_HOME=$(mktemp -d) codex exec resume 00000000-0000-4000-8000-000000000000 \
  --json --skip-git-repo-check -C /tmp hello </dev/null   # exit 2
CODEX_HOME=$(mktemp -d) codex exec -C /tmp resume 00000000-0000-4000-8000-000000000000 \
  --json --skip-git-repo-check -c model_provider=canned hello </dev/null   # exit 1, provider not found
```

## Redaction

The per-run scratch directory → `<SCRATCH>`. Nothing else in these files names the operator.
