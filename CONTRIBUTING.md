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

A test drives the harness first on `PATH`, and the gate (`marion_testsupport::on_path`) checks
that it is a release its row lists or one this machine's conformance battery admitted — see
"When a pinned harness drifts".

Suites that drive a real `claude`/`codex`/… skip loudly when the binary is absent. The
`--ignored` suites (`restart_resume`, `acp_child`) cost real tokens and stay opt-in.

`marion-supervisor`'s integration suites are modules of a few test binaries rather than one binary
each, so the crate links once per binary instead of once per suite:

- `it_live` (`tests/it_live/`): every suite that drives a real harness, i.e. gates on
  `on_path`, `harness_available` or `require_claude_and_codex`.
- `it_canned` (`tests/it_canned/`): every suite that needs no harness.
- binaries of their own: `endpoint_matrix`, `repo_trust`, `token_off_argv` and
  `user_agent_types`, which set process environment variables and so cannot share a process;
  `l45_tree`, the commit hook's target; and `conformance`, the opt-in battery.

A suite in a merged binary is named by its module path:

```sh
cargo test -p marion-supervisor --test it_canned journal::        # one suite
cargo test -p marion-supervisor --test it_live restart_resume:: -- --ignored
```

A new suite goes in `tests/it_live/` or `tests/it_canned/` with a `mod <suite>;` line in that
directory's `main.rs`, and reaches the shared bed as `crate::common`.

### The `client` feature

`marion-supervisor` builds two binaries from one library. The viewer's half of that library — the
home screen, `marion attach`, the tree, the native facade's keyboard relay, the setup form's
`agents.toml` editor — is behind its default `client` feature, together with the terminal emulator
(`marion-term`), `marion-tui`, `ratatui` and `similar`. `marion` requires the feature;
`marion-supervisor` does not use it. So the daemon, and the tests that only drive it, can skip that
whole stack:

```sh
cargo build -p marion-supervisor --no-default-features --bin marion-supervisor
cargo test -p marion-supervisor --no-default-features --lib --test endpoint_matrix
```

The test targets that need it say so with `required-features` in the crate's `Cargo.toml`:
`it_canned`, `it_live` and `conformance` run the `marion` binary or read a pane through the emulator,
and `l45_tree` snapshots the tree screen. Without `client` they are skipped rather than failed.
Everything else — the default `cargo build`, `cargo install`, the release — has the feature on.
A node's words (its label, short id, state) live in `node_label`, not `tree`, because the daemon
names nodes too.

## When a pinned harness drifts

The version gate is `marion_testsupport::on_path`, reading each row's `verified` list
(`HarnessSpec::verified`) — the only list. Every harness updates itself, so a release the list
does not name is also admitted when it passes the **basic tests**: the conformance battery, $0 against
marion's canned provider, judged against the cells the newest verified release scored in the
committed matrix. One command, once per release:

```sh
scripts/conformance.sh --harness claude-code      # the row's name; or --harness opencode, ...
```

It records the verdict for that exact binary under marion's state directory
(`<state>/conformance/<row>-<version>.json`), and from then on the gate drives it. A release
whose basics fail — or that was never probed, or whose record is another binary's — fails every
gated test naming the harness, the version and why; `marion doctor` says the same on its row.

`.github/workflows/canary.yml` does this every night on a macOS runner and needs no hand: it
installs the newest release of every harness (npm, and Homebrew for goose; no logins), runs the
battery on each unlisted one, runs the whole suite strict, and — only after a wholly green run —
opens one pull request adding every release the battery admitted to its row's `verified` list
(`scripts/admit-harness.sh`). An npm release is listed only once its publish is seven days old,
and never at or below a listed version. The pull request auto-merges where `main` has required
status checks and otherwise waits for a person; CI on it does not drive the harnesses, so those
checks are the gate, not CI. Any red — a regressed conformance
cell, a failing test — opens (or comments on) an issue labelled `harness-canary` naming the cells,
the failing tests and every installed version. Live cells — real model calls through a vendor
login — are outside it and stay manual.

For the canary's pull request, the owner once enables Settings → Actions → General → "Allow GitHub
Actions to create and approve pull requests" and Settings → General → "Allow auto-merge". A pull
request opened with the default token does not start `ci.yml`, so under the ruleset below it would
wait forever; adding a secret `CANARY_TOKEN` (a fine-grained token with Contents and Pull requests
write on this repository) makes CI run and the merge happen.

To list a release by hand — after reading why the battery held it back, say a P-approval failure
naming a moved ask key — `scripts/admit-harness.sh claude 2.1.301` adds it to its row's
`verified` list (it builds and runs nothing); re-run the battery for that row into the committed
fixtures so its transcripts become the bar the next release is held to.

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
cargo clippy -p marion-supervisor --no-default-features --all-targets -- -D warnings
cargo test --workspace --lib
scripts/three-axis.sh
```

## The three axes: generality, efficiency, security

Every change is judged on three axes before it lands, and `scripts/three-axis.sh` automates the
part a syntax tree can answer. It runs `cargo test -p marion-testsupport --test three_axis`,
which parses every shipped source file and fails on:

- **Generality** — branching on *which* harness outside the row files
  (`crates/marion-harness/src/<harness>.rs` and the `Harness` enum's own file): a `Harness::X`
  pattern or comparison, a harness-name string in a conditional, a baked-in agent-type name like
  `"codex-impl"`, a vendor env var (`ANTHROPIC_*`), a harness-named strategy variant, or a
  harness's `impl` living outside its row. Do instead: put the behaviour on the row as data, or
  in an enumerated strategy every row states, with a sweep test over `Harness::ALL`.
- **Efficiency** — timer-driven waiting in shipped code: `sleep` in a loop, `try_wait`/
  `try_recv` or a fixed-period `recv_timeout`/`wait_timeout` in a loop, a fixed read timeout, a
  `yield_now` spin, a `*_POLL`/`*_TICK` constant. Do instead: wait on the event (poll(2)/kqueue,
  a condvar, a blocking `recv()`, a pipe), and measure wakeups before and after.
- **Security** — a secret in a `Debug` derive or `Display` impl, a formatting/logging macro,
  argv, or a credential-bearing file written without `mode(0o600)`; and any test that spells a
  login or device flow. Do instead: a secret type with a redacting `Debug`, env instead of argv,
  `OpenOptionsExt::mode(0o600)`, and a BLOCKED report naming the login command for the user.

The security axis also runs `scripts/fixture-privacy.py` over `tests/fixtures/` (and
`scripts/conformance.sh` runs it on every capture): a home path, an email, or the name of a
skill, plugin or MCP server installed on your machine in a committed capture fails it. Scrub with
`--fix <tsv>` (same-length replacements) and record the pass in `tests/fixtures/REVIEW.md`.

Known findings live in `checks/{generality,efficiency,security}.allow` as `path:item  reason`.
The lists are a ratchet: a new finding fails the check, and so does an entry nothing matches any
more — a fix deletes its line in the same commit. Add an entry only with a reason a reviewer can
check (a bounded deadline, a verified false positive, or the tracked wave that removes it).

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

| crate | what it is |
|---|---|
| `marion-core` | The IR: launch specs, registry model, journal, task contract, and the client↔supervisor wire vocabulary. No processes, no filesystem. |
| `marion-harness` | One `HarnessSpec` row per harness (argv, env, tool spelling, resume shape, token carrier, update policy), each row naming the spike that measured it. |
| `marion-provider` | The canned model provider (`marion-canned`). Replays scripted responses across four wire formats, dispatching on request shape. |
| `marion-supervisor` | The supervisor and both binaries: PTY host, registry, socket, spawn path, ACP driver, doctor, native relay, home screen. |
| `marion-term` | A VT screen model for the display plane: a streaming grid with a ratatui adapter. |
| `marion-testsupport` | Shared test helpers, the version gate, and the three-axis checks. |
| `marion-tui` | The attach pane, the tree view and the home screen's widgets. |

`MILESTONES.md` owns *what* and *why*; `docs/specs/2026-07-31-marion-design.md` owns *how*;
where they disagree, that split decides. `docs/guide.md` is the user-facing reference behind the
README, and `docs/README.md` indexes the rest. `spikes/` holds the
throwaway probe scripts that produced the measurements, and `tests/fixtures/` holds their
recorded output — `tests/fixtures/REVIEW.md` is the redaction ledger for that corpus, and any
new fixture goes through it before it is committed.

`marion`'s commands are the rows of `VERBS` in
`crates/marion-supervisor/src/bin/marion/cli.rs`, each with its own `--help`; `marion --help` is
built from the table. A command parses its own flags through `Words`, taking `--repo`/`--state-dir`
from `Place` and `--canned`/`--base-url` from `Backend`, and refuses any flag it does not know.
Help and errors use plain words: no spec section numbers. An old spelling kept for a script or a
doc is an alias in the table (or an accepted, unlisted flag), never a second command.

## Releasing

Releases are built by [cargo-dist](https://github.com/axodotdev/cargo-dist) (`dist`, 0.33.0).
`dist-workspace.toml` is the configuration; `.github/workflows/release.yml` is generated from it
by `dist generate` and is never edited by hand. One archive per target (macOS, glibc Linux and
static musl Linux, each arm64 and x86_64) carries both `marion` and `marion-supervisor`, alongside a shell installer, a
Homebrew formula named `marion`, and the npm package `@ryankim17920/marion` (plain `marion` is
taken on npm), whose install step downloads the archive for the platform it runs on.

The Homebrew and npm publish jobs are the two exceptions to "generated": `publish-jobs` names
`./publish-homebrew` and `./publish-npm`, which are `.github/workflows/publish-homebrew.yml` and
`publish-npm.yml`, copies of dist's own jobs that print a notice and succeed when their token is
not set, instead of failing the release. (`dist` then warns that the Homebrew publish job is
disabled; it cannot see that the local job is that job.) After upgrading dist, compare the two
files with the jobs a fresh `dist generate` would write for `publish-jobs = ["homebrew", "npm"]`.

Pull requests run `dist plan` only. To build every target on a pull request without publishing,
set `pr-run-mode = "upload"` on a throwaway branch, run `dist generate`, and open a draft pull
request from it; the archives appear as workflow artifacts and nothing is released.

Before changing the configuration, check it locally:

```sh
dist plan                                             # what a release would contain
dist build --artifacts=local --target aarch64-apple-darwin   # one archive, for the host
dist generate --check                                 # release.yml matches the config
```

### Cutting a release

1. Bump `version` under `[workspace.package]` in `Cargo.toml`, run
   `cargo update --workspace --offline` so `Cargo.lock` follows, and commit. Never reuse a
   version whose tag exists: `v0.1.0` was made by hand and holds only the demo video, so the
   first cargo-dist release is `v0.2.0`.
2. Land that commit on `main` and wait for `main`'s CI to pass.
3. Tag the commit and push the tag (the tag's version must equal the workspace version):

   ```sh
   git tag -a v0.2.0 -m "marion v0.2.0" origin/main
   git push origin v0.2.0
   ```

The tag starts `release.yml`. What it publishes:

| Output | Where | Needs |
|---|---|---|
| Six archives (`marion-supervisor-<target>.tar.xz`, each holding `marion` and `marion-supervisor`) and their `.sha256` files | GitHub Release | nothing |
| `marion-supervisor-installer.sh`, `sha256.sum`, `source.tar.gz` | GitHub Release | nothing |
| `cargo binstall` (reads `[package.metadata.binstall]` against the archives above) | GitHub Release | nothing |
| `marion.rb` | pushed to `RyanKim17920/homebrew-tap` | `HOMEBREW_TAP_TOKEN` |
| `@ryankim17920/marion` | npmjs.com | `NPM_TOKEN` |

Without a token, its job shows a notice ("Homebrew formula not published" or "npm package not
published") on the run summary and succeeds; the formula and the npm tarball are still attached
to the GitHub Release. A tag with a pre-release suffix (`v0.3.0-rc.1`) makes a pre-release and
skips both publish jobs.

Every archive carries a signed SLSA build-provenance attestation (`github-attestations`), and the
npm package is published with `--provenance`. To check a download came from this repository's
release workflow:

```sh
gh attestation verify marion-supervisor-aarch64-apple-darwin.tar.xz --repo RyanKim17920/marion
npm audit signatures        # in a project that installed @ryankim17920/marion
```

Every action a workflow uses is pinned to a full commit SHA. In `release.yml` the pins come from
`[dist.github-action-commits]` in `dist-workspace.toml`, so `dist generate` keeps them; bump one
by resolving the new tag (`gh api repos/<owner>/<repo>/git/ref/tags/<tag>`, then
`git/tags/<sha>` if that names an annotated tag) and regenerating. The one unpinned fetch left is
dist's own: the generated workflow installs dist with `curl … | sh` from the pinned v0.33.0
release URL and, in a container build, rustup the same way. dist has no option to check a hash
there, and a hand edit would be overwritten by `dist generate` and fail `dist plan`'s check, so it
stays as generated.

If a release fails part-way, delete the GitHub Release and the tag (`git push --delete origin
v0.2.0`), fix, and tag again; nothing outside GitHub was published unless a publish job ran.

### One-time setup the owner does outside this repository

Nothing here can do these. Until they are done, the Homebrew and npm jobs skip as above.

1. Create the public repository `RyanKim17920/homebrew-tap`, with a default branch. It may be
   empty; the first release adds `Formula/marion.rb`.
2. Create a fine-grained personal access token with **Contents: read and write** on that tap
   repository only, and add it to this repository as the Actions secret `HOMEBREW_TAP_TOKEN`
   (Settings → Secrets and variables → Actions).
3. On npmjs.com, as the `ryankim17920` user (the scope must match it or an organization of that
   name), create a **granular access token** with read and write on packages in the
   `@ryankim17920` scope, and add it as the Actions secret `NPM_TOKEN`. The first publish creates
   the public package `@ryankim17920/marion`.

4. Protect `main` and the release tags. A pushed tag starts `release.yml`, whose publish jobs
   hold `HOMEBREW_TAP_TOKEN` and `NPM_TOKEN`, and the release pattern matches any tag containing
   a version, so the tag ruleset covers every tag. Repository admins bypass both, so the owner
   keeps pushing and tagging as now; anyone or anything else (a collaborator, a workflow token,
   the canary's `CANARY_TOKEN`) can neither create a tag nor push to `main` except through a pull
   request whose CI passed. Run once, as the owner:

   ```sh
   gh api -X POST repos/RyanKim17920/marion/rulesets --input - <<'EOF'
   {
     "name": "main",
     "target": "branch",
     "enforcement": "active",
     "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
     "bypass_actors": [{"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}],
     "rules": [
       {"type": "deletion"},
       {"type": "non_fast_forward"},
       {"type": "pull_request", "parameters": {
         "required_approving_review_count": 0, "dismiss_stale_reviews_on_push": false,
         "require_code_owner_review": false, "require_last_push_approval": false,
         "required_review_thread_resolution": false}},
       {"type": "required_status_checks", "parameters": {
         "strict_required_status_checks_policy": false,
         "required_status_checks": [
           {"context": "fmt + clippy"},
           {"context": "tests (ubuntu-latest)"},
           {"context": "tests (macos-latest)"}]}}
     ]
   }
   EOF

   gh api -X POST repos/RyanKim17920/marion/rulesets --input - <<'EOF'
   {
     "name": "release tags",
     "target": "tag",
     "enforcement": "active",
     "conditions": {"ref_name": {"include": ["~ALL"], "exclude": []}},
     "bypass_actors": [{"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}],
     "rules": [{"type": "creation"}, {"type": "update"}, {"type": "deletion"}]
   }
   EOF

   gh api repos/RyanKim17920/marion/rulesets --jq '.[] | "\(.id) \(.name) \(.enforcement)"'
   ```

   `actor_id` 5 is the built-in Admin repository role. The check names are the `name:`s of
   `ci.yml`'s jobs; rename one there and the ruleset must follow, or every pull request waits on
   a check that never reports.

No repository-wide Actions setting needs to change. The workflow asks for `contents: write`
itself, which the repository's default of read-only workflow permissions allows, and no job opens
or approves a pull request (the formula is pushed straight to the tap with `HOMEBREW_TAP_TOKEN`),
so "Allow GitHub Actions to create and approve pull requests" can stay off.

After that, `brew install RyanKim17920/tap/marion`, `npm install -g @ryankim17920/marion` and the
`curl … | sh` line on the release page all work. A secret added after a release takes effect from
the next tag; to publish an existing release's formula or package, re-run that release's
`custom-publish-homebrew` or `custom-publish-npm` job from the Actions tab while the run's
artifacts are still retained (90 days by default).

## Windows

Windows users run marion under WSL 2, where it is a Linux program and every channel above works.
Native Windows is not supported, and is a port, not a build flag. What stands in the way,
measured against the current tree:

- **The pty host.** `marion-supervisor/src/pty.rs` and `pty/` drive harness TUIs through a POSIX
  pseudo-terminal (`openpty`, `setsid`, controlling-terminal ioctls, `termios` raw mode), and the
  native facade (`native_tty`, `native_relay`) verifies the operator's terminal by the same
  ioctls. Windows has ConPTY instead, with a different lifecycle and no controlling-terminal
  concept.
- **Unix domain sockets and what rides on them.** The supervisor socket (`socket.rs`), the
  detach/attach path and the native bootstrap use `AF_UNIX`, peer credentials
  (`getpeereid`/`SO_PEERCRED`) for authentication, and `SCM_RIGHTS` to pass terminal descriptors
  between processes. Windows has `AF_UNIX` without descriptor passing or peer credentials; named
  pipes plus `DuplicateHandle` and the pipe client's process id are the equivalents.
- **Process groups and signals.** Kill, timeout and reap (`kill.rs`, `run.rs`, `detach.rs`) signal
  whole process groups and mask signals per thread. The Windows shape is a Job Object per node
  and `GenerateConsoleCtrlEvent`/`TerminateJobObject`.
- **Around the edges:** hand-declared `tcgetattr`/`tcsetattr` raw mode in `marion-tui`, `flock`
  on the journal, `/tmp`-style scratch paths, and every test fixture that
  opens a pty.

Rough effort: several weeks of focused work before `marion run` and `marion attach` pass on
Windows, most of it in the pty and socket layers and their tests, and more before the native
facade does. The lanes whose harnesses are themselves Unix-only gain nothing.

Suggested approach, if it is ever wanted: first turn `pty` and `socket` into seams — one trait
each for "spawn a child on a terminal, read, write, resize, wait" and "listen, accept, identify
the peer, pass a terminal" — with today's POSIX code as the only implementation and no behaviour
change. Then add ConPTY and named-pipe implementations behind them, a Job Object behind `kill`,
and a Windows runner in `ci.yml`. The native facade's descriptor-passing capability would need
its own design on Windows rather than a translation.
