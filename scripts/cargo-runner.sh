#!/bin/sh
# The cargo `runner` for this workspace (`.cargo/config.toml`): every binary cargo executes —
# `cargo test`, `cargo run`, `cargo bench` — starts with an EMPTY per-process directory first on
# PATH, named by MARION_HARNESS_SHIM. `marion-testsupport` fills it, once per process, with one
# symlink per pinned harness whose admitted release is still on disk, so a test that spawns
# `claude` gets the release the suite's conclusions were measured on rather than whatever an
# auto-update left first on PATH. The gate still probes `--version` and still refuses drift; only
# the choice of binary is taken back. See crates/marion-testsupport/src/shim.rs.
#
# The directory is made HERE and not by the test process because the process cannot prepend to
# its own PATH without `set_var`, a data race against every other test thread that is spawning
# `git` or `ps` at that moment. An empty directory ahead of PATH costs a `cargo run` nothing.
#
# The runner stays the parent (no `exec`) so the directory is removed however the binary ends.
set -u
root="/tmp/mn-$(id -u)"
mkdir -p -m 700 "$root" || exit 1
dir=$(mktemp -d "$root/shim-XXXXXX") || exit 1
trap 'rm -rf "$dir"' EXIT
MARION_HARNESS_SHIM="$dir" PATH="$dir:$PATH" "$@"
