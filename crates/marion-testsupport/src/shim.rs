//! **The harness shim: a `PATH` prefix that `exec`s the release the suite was measured on.**
//!
//! The version gate ([`crate::on_path`]) says *which* binary a test may drive; it cannot make that
//! binary the one `claude` resolves to. Three of the pinned harnesses auto-updated overnight on
//! 2026-09-06 (claude 2.1.261 → 2.1.263, codex 0.147.0 → 0.153.4, opencode 1.17.3 → 1.18.29), and
//! every gated suite went red at once — not because anything about marion changed, but because the
//! first `claude` on `PATH` was no longer one the table admits. Two of the three installers keep the
//! previous release on disk beside the new one, so the admitted binary was still there; nothing put
//! it first on `PATH`.
//!
//! This module does. A [`Shim`] owns one directory, and for each pinned harness whose admitted
//! release is still on disk it drops a symlink named for the program into that directory. With the
//! directory first on `PATH`, `Command::new("claude")` — from the test process, from a `marion`
//! binary it spawned, from a supervisor that binary detached — resolves to the pinned release, and
//! the gate, which now probes `--version` *through* the shim, passes for the right reason.
//!
//! # Why the directory is made by the cargo runner and not by this crate
//!
//! Most of the suite spawns a harness from the **test process's own environment**: `run_spawn` in
//! `harness_matrix`, `root::prepare` in `permission_round_trip`, and every `marion` binary a test
//! starts without an explicit `.env("PATH", …)`. The one way to put a directory ahead of *that*
//! `PATH` from inside the process is `std::env::set_var`, which is a data race against every other
//! test thread that is concurrently spawning `git` or `ps` — the reason `check_version` is a pure
//! function in the first place. So the directory is created **before the process starts**, by
//! `scripts/cargo-runner.sh` (installed as the `runner` in `.cargo/config.toml`): an empty
//! per-process directory, first on `PATH`, named by `MARION_HARNESS_SHIM`, removed when the process
//! ends. This crate only *fills* it, once per process, the first time a test asks for a pinned
//! harness — a symlink appearing in a directory that is already on `PATH` is not an environment
//! mutation, and nothing racing it can observe an inconsistent state.
//!
//! # What each store looks like
//!
//! Measured on darwin 25.5.0, 2026-09-06:
//!
//! - **claude** keeps every downloaded release as a single executable,
//!   `~/.local/share/claude/versions/<ver>`, and `~/.local/bin/claude` is a symlink the updater
//!   repoints. Four releases were on disk (2.1.252, 2.1.259, 2.1.261, 2.1.263).
//! - **codex** keeps `~/.codex/packages/standalone/releases/<ver>-<target>/bin/codex` — the
//!   directory carries the target triple (`0.147.0-aarch64-apple-darwin`) — and
//!   `~/.codex/packages/standalone/current` is the repointed symlink. Five releases were on disk.
//! - **npm-installed** harnesses (copilot, gemini, qwen, cline, all under Homebrew's node tree on
//!   this machine) have no per-version store: `npm i -g` overwrites in place. For those the shim
//!   keeps its own, `<marion state>/harness-pins/<program>/<ver>`, filled by
//!   `npm install -g --prefix` **only** when the binary first on `PATH` is not admitted — a network
//!   step, so it is never taken for a harness the gate would pass anyway.
//! - **Homebrew formulae** (opencode, goose) keep only the current version under `Cellar`; there
//!   is nothing to pin to. Those fall through to `PATH`, and the gate speaks.
//!
//! # What "fall through" means
//!
//! A harness with no admitted release on disk is **not** shimmed, and the gate probes whatever is
//! first on the inherited `PATH`. If that is admitted, fine. If not, the diagnosis is the same one
//! the gate always gave — both versions named, the table named — with one sentence added: *no
//! pinned release on disk*, and where this looked. A shim that hid the drift by refusing to run
//! would be the silent pass in a new coat; the drift stays loud, and only the *choice* of binary is
//! taken away from the auto-updater.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::{PINNED_HARNESSES, PinnedHarness, check_version, parse_version};

/// Where a harness's installer keeps releases side by side — or that it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseStore {
    /// `~/.local/share/claude/versions/<ver>`, an executable per release.
    ClaudeVersions,
    /// `~/.codex/packages/standalone/releases/<ver>-<target>/bin/codex`.
    CodexReleases,
    /// An npm package with no store of its own; the shim keeps one under marion's state dir and
    /// installs `<pkg>@<ver>` into it on demand.
    Npm(&'static str),
    /// Nothing to pin to (a Homebrew formula): `PATH` decides, and the gate judges.
    PathOnly,
}

/// The roots the stores hang off: the user's home, and marion's own state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseStores {
    pub home: PathBuf,
    pub state: PathBuf,
}

impl ReleaseStores {
    /// The real machine: `$HOME`, and the state directory `marion` itself would use.
    pub fn from_env() -> Self {
        let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
        let state = marion_core::paths::state_dir(
            std::env::var("MARION_STATE_DIR").ok().as_deref(),
            std::env::var("XDG_STATE_HOME").ok().as_deref(),
            home.as_deref(),
        )
        .expect("a state directory resolves from MARION_STATE_DIR, XDG_STATE_HOME or HOME");
        Self {
            home: PathBuf::from(home.expect("HOME is set")),
            state,
        }
    }

    /// The private prefix an npm pin is installed into.
    fn npm_prefix(&self, program: &str, version: &str) -> PathBuf {
        self.state.join("harness-pins").join(program).join(version)
    }

    /// The executable for `version` of `program`, if its store still has it. Never runs anything.
    fn locate(&self, pin: &PinnedHarness, version: &str) -> Option<PathBuf> {
        let candidate = match pin.store {
            ReleaseStore::ClaudeVersions => {
                self.home.join(".local/share/claude/versions").join(version)
            }
            ReleaseStore::CodexReleases => {
                let releases = self.home.join(".codex/packages/standalone/releases");
                let prefix = format!("{version}-");
                let dir = std::fs::read_dir(&releases).ok()?.flatten().find(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name == version || name.starts_with(&prefix)
                })?;
                dir.path().join("bin").join(pin.program)
            }
            ReleaseStore::Npm(_) => self
                .npm_prefix(pin.program, version)
                .join("bin")
                .join(pin.program),
            ReleaseStore::PathOnly => return None,
        };
        candidate.is_file().then_some(candidate)
    }

    /// Where [`Self::locate`] looked, for the diagnosis when it found nothing.
    fn describe(&self, pin: &PinnedHarness) -> String {
        match pin.store {
            ReleaseStore::ClaudeVersions => format!(
                "{}/<ver>",
                self.home.join(".local/share/claude/versions").display()
            ),
            ReleaseStore::CodexReleases => format!(
                "{}/<ver>-<target>/bin/codex",
                self.home
                    .join(".codex/packages/standalone/releases")
                    .display()
            ),
            ReleaseStore::Npm(pkg) => format!(
                "{}/<ver>/bin/{} (npm package {pkg})",
                self.state.join("harness-pins").join(pin.program).display(),
                pin.program
            ),
            ReleaseStore::PathOnly => {
                format!("nowhere: `{}` has no per-version store", pin.program)
            }
        }
    }
}

/// How one pinned harness resolves through the shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// An admitted release is on disk and the shim's `<program>` symlink points at it.
    Pinned {
        version: &'static str,
        release: PathBuf,
    },
    /// No admitted release on disk; the inherited `PATH` decides, and `looked` says where this
    /// searched so the gate's diagnosis can.
    PathOnly { looked: String },
}

/// Why [`Shim::gate`] did not pass.
#[derive(Debug)]
pub enum GateFailure {
    /// `<program> --version` could not be run or did not exit 0: the binary is absent. Callers
    /// phrase this one themselves ("put `codex` on PATH").
    Absent,
    /// The binary ran and is not one the table admits; the diagnosis names both versions.
    Refused(String),
}

/// One directory of symlinks, filled lazily, one entry per pinned harness at most.
pub struct Shim {
    dir: PathBuf,
    stores: ReleaseStores,
    inherited_path: OsString,
    table: &'static [PinnedHarness],
    resolved: Mutex<BTreeMap<&'static str, Resolution>>,
}

impl Shim {
    /// `dir` is created if missing and is expected to be empty of harness names; `inherited_path`
    /// is what `PATH` would have been without the shim.
    pub fn new(
        dir: PathBuf,
        stores: ReleaseStores,
        inherited_path: OsString,
        table: &'static [PinnedHarness],
    ) -> Self {
        std::fs::create_dir_all(&dir).expect("the shim directory exists");
        Self {
            dir,
            stores,
            inherited_path,
            table,
            resolved: Mutex::new(BTreeMap::new()),
        }
    }

    /// The directory the symlinks live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `PATH` with the shim first: what every harness spawn must resolve through.
    pub fn path(&self) -> OsString {
        // Spelled by hand: `inherited_path` is already a joined list, and `join_paths` refuses any
        // component carrying the separator.
        let mut path = self.dir.clone().into_os_string();
        if !self.inherited_path.is_empty() {
            path.push(":");
            path.push(&self.inherited_path);
        }
        path
    }

    fn pin(&self, program: &str) -> &'static PinnedHarness {
        self.table
            .iter()
            .find(|p| p.program == program)
            .unwrap_or_else(|| panic!("{program:?} is not a pinned harness; see PINNED_HARNESSES"))
    }

    /// Decide once, per process, how `program` resolves — and lay the symlink if it does.
    ///
    /// The **newest admitted** release on disk wins: the table's tail is what the suite was most
    /// recently observed green on, and preferring it means an admission of the version that is
    /// already first on `PATH` makes the shim and `PATH` agree rather than disagree.
    pub fn resolve(&self, program: &str) -> Resolution {
        let pin = self.pin(program);
        let mut resolved = self.resolved.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = resolved.get(pin.program) {
            return r.clone();
        }
        let r = self.resolve_uncached(pin);
        resolved.insert(pin.program, r.clone());
        r
    }

    fn resolve_uncached(&self, pin: &'static PinnedHarness) -> Resolution {
        let on_disk = |shim: &Self| {
            pin.accepted
                .iter()
                .rev()
                .find_map(|v| shim.stores.locate(pin, v).map(|release| (*v, release)))
        };
        let mut found = on_disk(self);
        if found.is_none()
            && let ReleaseStore::Npm(pkg) = pin.store
            && !self.path_binary_is_admitted(pin)
        {
            let version = pin.accepted[pin.accepted.len() - 1];
            self.npm_install(pin, pkg, version);
            found = on_disk(self);
        }
        match found {
            Some((version, release)) => {
                let link = self.dir.join(pin.program);
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(&release, &link).unwrap_or_else(|e| {
                    panic!("symlink {} -> {}: {e}", link.display(), release.display())
                });
                Resolution::Pinned { version, release }
            }
            None => Resolution::PathOnly {
                looked: self.stores.describe(pin),
            },
        }
    }

    /// Does the `program` first on the *inherited* `PATH` already satisfy the table? Asked only
    /// before an npm install, because that is the one step with a cost worth avoiding.
    fn path_binary_is_admitted(&self, pin: &PinnedHarness) -> bool {
        Command::new(pin.program)
            .env("PATH", &self.inherited_path)
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                parse_version(&String::from_utf8_lossy(&o.stdout))
                    .map(|v| pin.accepted.contains(&v))
            })
            .unwrap_or(false)
    }

    /// `npm install -g --prefix <state>/harness-pins/<program>/<ver> <pkg>@<ver>`. Network. A
    /// failure is reported and swallowed: the harness then falls through to `PATH`, and the gate's
    /// diagnosis says the pin is not on disk.
    fn npm_install(&self, pin: &PinnedHarness, pkg: &str, version: &str) {
        let prefix = self.stores.npm_prefix(pin.program, version);
        let _ = std::fs::create_dir_all(&prefix);
        eprintln!(
            "marion-testsupport: `{}` first on PATH is not admitted and no pinned release is on \
             disk; installing {pkg}@{version} into {}",
            pin.program,
            prefix.display()
        );
        let out = Command::new("npm")
            .args([
                "install",
                "-g",
                "--no-fund",
                "--no-audit",
                "--loglevel",
                "error",
            ])
            .arg("--prefix")
            .arg(&prefix)
            .arg(format!("{pkg}@{version}"))
            .env("PATH", &self.inherited_path)
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => eprintln!(
                "marion-testsupport: npm install {pkg}@{version} failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!("marion-testsupport: npm could not be run: {e}"),
        }
    }

    /// The version gate, probed **through the shim**: `<program> --version` with [`Self::path`],
    /// judged by [`check_version`], and — when nothing was pinned — told where this looked.
    pub fn gate(&self, program: &str) -> Result<(), GateFailure> {
        let pin = self.pin(program);
        let resolution = self.resolve(program);
        let out = Command::new(program)
            .env("PATH", self.path())
            .arg("--version")
            .output()
            .map_err(|_| GateFailure::Absent)?;
        if !out.status.success() {
            return Err(GateFailure::Absent);
        }
        check_version(
            pin,
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        )
        .map_err(|diagnosis| match resolution {
            Resolution::PathOnly { looked } => GateFailure::Refused(format!(
                "{diagnosis}\nThe binary that answered is the one first on PATH, because there is \
                 no pinned release on disk to shim in ahead of it (looked in {looked})."
            )),
            Resolution::Pinned { version, release } => GateFailure::Refused(format!(
                "{diagnosis}\nThe shim pointed `{program}` at {} expecting {version}, and it did \
                 not answer with that version.",
                release.display()
            )),
        })
    }
}

/// The env var `scripts/cargo-runner.sh` sets to the per-process shim directory it made.
pub const SHIM_DIR_VAR: &str = "MARION_HARNESS_SHIM";

/// The one shim this process fills: the runner's directory, the real stores, the real table.
///
/// Panics — rather than quietly probing `PATH` — when the runner did not run, because a gate that
/// passed against `PATH` while the harness the test then spawns is the same `PATH`'s is exactly
/// the situation this module exists to end; it would just be *correct* by coincidence, and wrong
/// the day the pinned release differs from the installed one.
pub fn process_shim() -> &'static Shim {
    static SHIM: OnceLock<Shim> = OnceLock::new();
    SHIM.get_or_init(|| {
        let Some(dir) = std::env::var_os(SHIM_DIR_VAR).filter(|d| !d.is_empty()) else {
            panic!(
                "{SHIM_DIR_VAR} is not set, so no harness shim directory is on this process's \
                 PATH. Run the suite through cargo from the workspace root, whose \
                 `.cargo/config.toml` installs `scripts/cargo-runner.sh` as the runner; a test \
                 binary started any other way would drive whatever `claude` an auto-update left \
                 first on PATH while the gate reported the pinned one."
            );
        };
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut components = std::env::split_paths(&path);
        assert_eq!(
            components.next().as_deref(),
            Some(Path::new(&dir)),
            "{SHIM_DIR_VAR}={dir:?} is set but is not the first PATH entry ({path:?}); the shim \
             is only a shim when nothing resolves ahead of it"
        );
        let inherited: OsString =
            std::env::join_paths(components).expect("PATH's own components carry no separator");
        Shim::new(
            PathBuf::from(dir),
            ReleaseStores::from_env(),
            inherited,
            PINNED_HARNESSES,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch;
    use std::os::unix::fs::PermissionsExt;

    /// A fake release: an executable that prints `--version` the way the real one does.
    fn fake_release(path: &Path, prints: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{prints}'\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn table(
        program: &'static str,
        accepted: &'static [&'static str],
        store: ReleaseStore,
    ) -> &'static [PinnedHarness] {
        Box::leak(Box::new([PinnedHarness {
            program,
            accepted,
            store,
        }]))
    }

    fn shim_in(dir: &Path, table: &'static [PinnedHarness]) -> Shim {
        Shim::new(
            dir.join("shim"),
            ReleaseStores {
                home: dir.join("home"),
                state: dir.join("state"),
            },
            dir.join("bin").into_os_string(),
            table,
        )
    }

    fn version_through(shim: &Shim, program: &str) -> String {
        let out = Command::new(program)
            .env("PATH", shim.path())
            .arg("--version")
            .output()
            .expect("the program resolves through the shim's PATH");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn refused(r: Result<(), GateFailure>) -> String {
        match r {
            Err(GateFailure::Refused(d)) => d,
            other => panic!("expected the gate to refuse with a diagnosis, got {other:?}"),
        }
    }

    /// **The case that happened on 2026-09-06.** 2.1.263 is first on PATH; the table admits
    /// 2.1.220 and that release is still in claude's versions store. The shim must win.
    #[test]
    fn the_shim_execs_the_pinned_release_ahead_of_a_newer_binary_first_on_path() {
        let dir = scratch("shim-pinned");
        let release = dir.join("home/.local/share/claude/versions/2.1.220");
        fake_release(&release, "2.1.220 (Claude Code)");
        fake_release(&dir.join("bin/claude"), "2.1.999 (Claude Code)");
        let shim = shim_in(
            &dir,
            table("claude", &["2.1.220"], ReleaseStore::ClaudeVersions),
        );

        shim.gate("claude")
            .expect("the pinned release is on disk, so the gate passes through the shim");
        assert_eq!(version_through(&shim, "claude"), "2.1.220 (Claude Code)");
        assert_eq!(
            shim.resolve("claude"),
            Resolution::Pinned {
                version: "2.1.220",
                release
            }
        );
    }

    /// **The other half: the shim never hides drift.** No pinned release on disk means the newer
    /// binary answers, the gate refuses exactly as before, and the diagnosis says why the shim
    /// could not help.
    #[test]
    fn an_absent_pinned_release_falls_through_to_the_gate_which_says_so() {
        let dir = scratch("shim-absent");
        fake_release(&dir.join("bin/claude"), "2.1.999 (Claude Code)");
        let shim = shim_in(
            &dir,
            table("claude", &["2.1.220"], ReleaseStore::ClaudeVersions),
        );

        let e = refused(shim.gate("claude"));
        assert!(e.contains("2.1.999"), "names what answered: {e}");
        assert!(e.contains("2.1.220"), "names what the table admits: {e}");
        assert!(e.contains("no pinned release on disk"), "{e}");
        assert!(e.contains("claude/versions"), "says where it looked: {e}");
        assert!(
            !dir.join("shim/claude").exists(),
            "nothing was shimmed, so PATH's own binary is what answered"
        );
    }

    /// codex's store carries the target triple in the directory name, and three versions are
    /// admitted: the newest admitted release on disk is the one shimmed, not the pin (entry zero)
    /// and not the newest release present.
    #[test]
    fn the_newest_admitted_release_wins_and_codex_dirs_are_matched_by_version_prefix() {
        let dir = scratch("shim-codex");
        let releases = dir.join("home/.codex/packages/standalone/releases");
        for v in ["0.146.0", "0.147.0", "0.153.4"] {
            fake_release(
                &releases.join(format!("{v}-aarch64-apple-darwin/bin/codex")),
                &format!("codex-cli {v}"),
            );
        }
        fake_release(&dir.join("bin/codex"), "codex-cli 0.153.4");
        let shim = shim_in(
            &dir,
            table(
                "codex",
                &["0.146.0", "0.146.1", "0.147.0"],
                ReleaseStore::CodexReleases,
            ),
        );

        shim.gate("codex").expect("0.147.0 is admitted and on disk");
        assert_eq!(version_through(&shim, "codex"), "codex-cli 0.147.0");
    }

    /// An npm harness is read from marion's private prefix first, with no network and no probe.
    #[test]
    fn an_npm_pin_already_in_marions_prefix_is_used_without_installing() {
        let dir = scratch("shim-npm");
        fake_release(
            &dir.join("state/harness-pins/copilot/1.0.83/bin/copilot"),
            "GitHub Copilot CLI 1.0.83.",
        );
        fake_release(&dir.join("bin/copilot"), "GitHub Copilot CLI 1.0.99.");
        let shim = shim_in(
            &dir,
            table("copilot", &["1.0.83"], ReleaseStore::Npm("@github/copilot")),
        );

        shim.gate("copilot")
            .expect("the private prefix holds the pin");
        assert_eq!(
            version_through(&shim, "copilot"),
            "GitHub Copilot CLI 1.0.83."
        );
    }

    /// An npm harness whose PATH binary is admitted is left alone: no prefix is created, because
    /// the install is the one step that costs a network round trip.
    #[test]
    fn an_admitted_npm_binary_on_path_is_not_reinstalled() {
        let dir = scratch("shim-npm-ok");
        fake_release(&dir.join("bin/copilot"), "GitHub Copilot CLI 1.0.83.");
        let shim = shim_in(
            &dir,
            table("copilot", &["1.0.83"], ReleaseStore::Npm("@github/copilot")),
        );

        shim.gate("copilot").expect("PATH's copilot is admitted");
        assert!(
            !dir.join("state/harness-pins").exists(),
            "nothing was installed for a binary the gate already passes"
        );
        assert!(!dir.join("shim/copilot").exists());
    }

    /// A Homebrew formula has no store: PATH decides, and the gate alone judges, both ways.
    #[test]
    fn a_harness_with_no_store_is_taken_from_path_and_the_gate_alone_decides() {
        let ok = scratch("shim-brew-ok");
        fake_release(&ok.join("bin/opencode"), "1.17.3");
        shim_in(&ok, table("opencode", &["1.17.3"], ReleaseStore::PathOnly))
            .gate("opencode")
            .expect("PATH's opencode is the pin");

        let drifted = scratch("shim-brew-drift");
        fake_release(&drifted.join("bin/opencode"), "1.18.29");
        let e = refused(
            shim_in(
                &drifted,
                table("opencode", &["1.17.3"], ReleaseStore::PathOnly),
            )
            .gate("opencode"),
        );
        assert!(e.contains("1.18.29") && e.contains("1.17.3"), "{e}");
        assert!(e.contains("no pinned release on disk"), "{e}");
        assert!(e.contains("no per-version store"), "{e}");
    }

    /// A binary that is not there at all is `Absent`, which callers phrase themselves.
    #[test]
    fn a_missing_binary_is_absent_rather_than_refused() {
        let dir = scratch("shim-missing");
        let shim = shim_in(&dir, table("goose", &["1.49.0"], ReleaseStore::PathOnly));
        assert!(matches!(shim.gate("goose"), Err(GateFailure::Absent)));
    }

    /// **The wiring, as a test rather than a README line.** Under cargo the runner has made this
    /// process's shim directory and put it first on PATH before `main` ran.
    #[test]
    fn under_cargo_the_runner_puts_this_processes_shim_directory_first_on_path() {
        let dir = std::env::var_os(SHIM_DIR_VAR).unwrap_or_else(|| {
            panic!(
                "{SHIM_DIR_VAR} unset: `.cargo/config.toml` must install scripts/cargo-runner.sh"
            )
        });
        assert!(
            Path::new(&dir).is_dir(),
            "{dir:?} exists for the life of the process"
        );
        let path = std::env::var_os("PATH").unwrap_or_default();
        assert_eq!(
            std::env::split_paths(&path).next().as_deref(),
            Some(Path::new(&dir)),
            "the shim directory is the first PATH entry"
        );
        let shim = process_shim();
        assert_eq!(shim.dir(), Path::new(&dir));
    }

    /// The real inherited PATH is a joined list, which `std::env::join_paths` refuses as a single
    /// component — every real cell failed on exactly that before this test existed. The shim
    /// spells the concatenation itself and must keep every entry, in order, behind its own.
    #[test]
    fn the_shims_path_keeps_every_inherited_entry_behind_the_shim_directory() {
        let dir = scratch("shim-path");
        let shim = Shim::new(
            dir.join("shim"),
            ReleaseStores {
                home: dir.join("home"),
                state: dir.join("state"),
            },
            OsString::from("/usr/bin:/bin"),
            table("goose", &["1.49.0"], ReleaseStore::PathOnly),
        );
        let parts: Vec<PathBuf> = std::env::split_paths(&shim.path()).collect();
        assert_eq!(
            parts,
            [
                dir.join("shim"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin")
            ]
        );
    }
}
