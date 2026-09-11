# Contributing to marion

## Toolchain

`rust-toolchain.toml` pins **1.94.0** with `rustfmt` and `clippy`. rustup reads it
automatically; do not override the channel. Edition 2024, `resolver = "3"`.

macOS and Linux only. Several suites need a real PTY and Unix domain sockets, and a few of the
socket tests fail when the checkout path is long enough to exhaust `sun_path` (104 bytes on
macOS) — that is the environment, not the change.

## Running tests

```sh
cargo test --workspace          # everything
cargo test --workspace --lib    # unit tests only, no integration binaries
```

**Always through cargo, never a raw test binary.** `.cargo/config.toml` sets
`runner = "scripts/cargo-runner.sh"`, so every binary cargo executes starts with an empty
per-process directory first on `PATH`, named by `MARION_HARNESS_SHIM`. `marion-testsupport`
fills that directory with one symlink per pinned harness. A test invoked outside the runner
spawns whatever an auto-update left first on `PATH`, and its conclusions are then about a
release nobody measured.

Suites that drive a real `claude`/`codex`/… skip loudly when the binary is absent. The
`--ignored` suites (`restart_resume`, `acp_child`) cost real tokens and stay opt-in.

## When a pinned harness drifts

The version gate is `marion_testsupport::PINNED_HARNESSES`. When a harness auto-updates, the
gate goes red and names both versions. **The fix is never to widen the set.** It is to re-run
everything that drives that harness against the new version and admit it with the evidence:

```sh
scripts/admit-harness.sh claude 2.1.263
scripts/admit-harness.sh opencode 1.18.30 goose 1.50.0   # two at once when both drifted
```

The script adds each version to its `accepted` list (entry zero — the pin — is never touched),
runs the table's own tests plus every gated suite that names the harness, and on all green
writes the dated observation comment and prints the MILESTONES paragraph and commit message. On
any red it restores the table exactly and exits non-zero. It does not commit; read the diff
first.

## The commit gate

L4.5 — the snapshot layer — gates *commits*, not pushes. Install it once per clone:

```sh
git config core.hooksPath .githooks
```

`.githooks/pre-commit` runs `cargo test -p marion-term --test l45_driver` and
`cargo test -p marion-supervisor --test l45_tree`, checks the pass count back so a renamed
target cannot silently no-op the gate, and blocks the commit on either failing. It names those
two targets explicitly so no future test file is swept into the gate by existing. A snapshot
diff is reviewed with `cargo insta review`, never accepted blind.

Before pushing, run what CI runs:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --lib
```

## Commits

Format is `type: description` — `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`.
Explain the *why*, not just the *what*.

**Micro-commits.** One independently testable behaviour or invariant per commit. Unrelated
cleanup gets its own commit; feature work, review fixes and cleanup are never squashed together.

**`MILESTONES.md` is the ledger, and it moves in the same commit.** Any change that affects a
milestone marker, a verified harness fact, a pinned version, or the "Where it actually stands"
section updates `MILESTONES.md` in that same commit — or states in the message why the marker is
unchanged. A milestone doc that trails the code by even one commit has already lied once.

## Where things are

`MILESTONES.md` owns *what* and *why*; `docs/specs/2026-07-31-marion-design.md` owns *how*;
where they disagree, that split decides. `docs/README.md` indexes the rest. `spikes/` holds the
throwaway probe scripts that produced the measurements, and `tests/fixtures/` holds their
recorded output — `tests/fixtures/REVIEW.md` is the redaction ledger for that corpus, and any
new fixture goes through it before it is committed.
