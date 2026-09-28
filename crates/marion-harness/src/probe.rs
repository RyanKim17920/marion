//! `<program> --version`, run the one way marion may run a real harness to read its version:
//! **carrying the row's no-self-update switch**, whatever shape that switch takes.
//!
//! Running a harness bare is not harmless. gemini 0.53.0 started without its settings document
//! updated the operator's global install to 0.61.0; copilot 1.0.83 asked `--version` without
//! `COPILOT_AUTO_UPDATE=false` downloaded a newer build and answered with *that* build's version,
//! which no node marion spawns runs. So the
//! switch is applied here from the row's [`UpdatePolicy`] data, in the same spelling the row's
//! launches use — never typed per harness — and a row whose binary may update itself with no known
//! switch is **refused**, not probed bare.
//!
//! | policy                      | the probe carries                                              |
//! |-----------------------------|----------------------------------------------------------------|
//! | [`UpdatePolicy::Env`]       | the variable                                                   |
//! | [`UpdatePolicy::Pair`]      | `<flag> key=value` on the row's own override channel — the flag of its `Arg::Each(_, Field::Pairs)` (`EachEq` spells `<flag>=key=value`) |
//! | [`UpdatePolicy::Document`]  | a private document holding the keys ([`UpdatePolicy::apply_to_json`]), named the way the row's [`LiveDeclaration`] names its document |
//! | [`UpdatePolicy::Never`]     | nothing: the binary never updates itself                       |
//! | [`UpdatePolicy::None`]      | refused ([`ProbeRefusal::NoSwitch`])                           |
//!
//! **`--version` comes first, and the switch's arguments after it.** Every argument parser marion
//! drives either reads the whole argv before acting (so the order is immaterial) or acts on
//! `--version` the moment it meets it — printing and exiting before any configuration, and so any
//! updater, is reached (codex's clap). Neither gives a switch placed first anything a switch placed
//! after lacks, while `--version` first keeps `$1` the question for everything that dispatches on
//! it — the convention every fake harness in this workspace's tests answers by.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::spec::{Arg, Field, HarnessSpec, LiveDeclaration, UpdatePolicy};

/// The file name of a [`UpdatePolicy::Document`] probe's settings document, inside its private
/// directory.
const DOCUMENT_FILE: &str = "settings.json";

/// How a probe of one row's program carries its no-self-update switch — the row's policy resolved
/// against the row's own channels, before anything is written or run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeSwitch {
    /// Set this variable.
    Env { key: String, value: String },
    /// Put these arguments ahead of `--version`: a pair on the row's override channel.
    Args(Vec<String>),
    /// Write `body` to a private file and name its path the way `via` says.
    Document { via: DocumentChannel, body: String },
    /// Nothing to carry: the binary never updates itself ([`UpdatePolicy::Never`]).
    Unneeded,
}

/// How a probe names its settings document — the channel the row's [`LiveDeclaration`] uses for
/// the document its launches write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentChannel {
    /// The path is the value of this variable (gemini's `GEMINI_CLI_SYSTEM_SETTINGS_PATH`).
    Env(&'static str),
    /// `<flag> <prefix><path>` on argv.
    Argv {
        flag: &'static str,
        prefix: &'static str,
    },
}

/// Why a row's program may not be probed: running it could let it update itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProbeRefusal {
    /// [`UpdatePolicy::None`]: the binary may update itself and no switch is known.
    #[error("no no-self-update switch is known for this harness ({note}), so it is not run")]
    NoSwitch { note: &'static str },
    /// [`UpdatePolicy::Pair`] on a row with no `Field::Pairs` slot in any argv shape.
    #[error("the row's update pair has no override channel on its argv")]
    NoPairChannel,
    /// [`UpdatePolicy::Document`] on a row whose declaration names no document by env or argv.
    #[error("the row's update document has no channel naming its path")]
    NoDocumentChannel,
}

impl ProbeSwitch {
    /// Resolve `spec`'s [`UpdatePolicy`] against the row's own channels.
    pub fn for_row(spec: &HarnessSpec) -> Result<Self, ProbeRefusal> {
        match spec.updates {
            UpdatePolicy::Env { key, value, .. } => Ok(Self::Env {
                key: key.to_string(),
                value: value.to_string(),
            }),
            UpdatePolicy::Pair { key, value, .. } => pair_args(spec, &format!("{key}={value}"))
                .map(Self::Args)
                .ok_or(ProbeRefusal::NoPairChannel),
            UpdatePolicy::Document { .. } => {
                let via = match spec.live_declaration {
                    Some(LiveDeclaration::EnvDocument { key, .. }) => DocumentChannel::Env(key),
                    Some(LiveDeclaration::ArgvDocument { flag, prefix, .. }) => {
                        DocumentChannel::Argv { flag, prefix }
                    }
                    _ => return Err(ProbeRefusal::NoDocumentChannel),
                };
                let mut doc = serde_json::Value::Object(Default::default());
                spec.updates.apply_to_json(&mut doc);
                Ok(Self::Document {
                    via,
                    body: doc.to_string(),
                })
            }
            UpdatePolicy::Never { .. } => Ok(Self::Unneeded),
            UpdatePolicy::None { note } => Err(ProbeRefusal::NoSwitch { note }),
        }
    }
}

/// The row's override channel spelling one `pair`: the first `Field::Pairs` slot in its headless
/// argv, else in its pane argv.
fn pair_args(spec: &HarnessSpec, pair: &str) -> Option<Vec<String>> {
    spec.argv
        .iter()
        .chain(spec.pane.unwrap_or_default())
        .find_map(|arg| match *arg {
            Arg::Each(flag, Field::Pairs) => Some(vec![flag.to_string(), pair.to_string()]),
            Arg::EachEq(flag, Field::Pairs) => Some(vec![format!("{flag}={pair}")]),
            _ => None,
        })
}

/// A ready `<program> --version` and whatever it needs to outlive the run: keep this alive until
/// the child has exited, because dropping it removes a document probe's settings file.
#[derive(Debug)]
pub struct VersionProbe {
    pub command: Command,
    _document: Option<PrivateDir>,
}

/// Why no probe command was built.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error(transparent)]
    Refused(#[from] ProbeRefusal),
    #[error("could not write the probe's settings document: {0}")]
    Document(#[from] io::Error),
}

/// `<program> --version` under `spec`'s no-self-update switch — the one constructor every marion
/// `--version` probe of a harness uses.
pub fn version_probe(
    spec: &HarnessSpec,
    program: impl AsRef<OsStr>,
) -> Result<VersionProbe, ProbeError> {
    let switch = ProbeSwitch::for_row(spec)?;
    let mut command = Command::new(program);
    command.arg("--version");
    let mut document = None;
    match switch {
        ProbeSwitch::Env { key, value } => {
            command.env(key, value);
        }
        ProbeSwitch::Args(args) => {
            command.args(args);
        }
        ProbeSwitch::Document { via, body } => {
            let dir = PrivateDir::new()?;
            let path = dir.write(DOCUMENT_FILE, body.as_bytes())?;
            match via {
                DocumentChannel::Env(key) => {
                    command.env(key, &path);
                }
                DocumentChannel::Argv { flag, prefix } => {
                    let mut value = OsString::from(prefix);
                    value.push(&path);
                    command.arg(flag).arg(value);
                }
            }
            document = Some(dir);
        }
        ProbeSwitch::Unneeded => {}
    }
    Ok(VersionProbe {
        command,
        _document: document,
    })
}

/// A directory only this user can enter (0700), created fresh under the system temp dir and
/// removed on drop. `create` rather than `create_all`, so an existing path — someone else's
/// directory or symlink planted at the name — is an error, never reused.
#[derive(Debug)]
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn new() -> io::Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "marion-version-probe-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    /// Write `body` to a new 0600 file `name` inside the directory, returning its path.
    fn write(&self, name: &str, body: &[u8]) -> io::Result<PathBuf> {
        let path = self.0.join(name);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?
            .write_all(body)?;
        Ok(path)
    }

    #[cfg(test)]
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::harness_spec;
    use marion_core::harness::Harness;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn envs(command: &Command) -> Vec<(String, String)> {
        command
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect()
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// **Every row either gets a probe carrying its switch or is refused by name** — no row is
    /// probed bare unless it states its binary never updates itself.
    #[test]
    fn every_row_is_probed_with_its_switch_or_refused() {
        for h in Harness::ALL {
            let spec = harness_spec(h);
            match (spec.updates, version_probe(spec, "/bin/x")) {
                (UpdatePolicy::Env { key, value, .. }, Ok(p)) => {
                    assert_eq!(envs(&p.command), [(key.into(), value.into())], "{h}");
                    assert_eq!(args(&p.command), ["--version"], "{h}");
                }
                (UpdatePolicy::Pair { key, value, .. }, Ok(p)) => {
                    let a = args(&p.command);
                    assert_eq!(a.first().map(String::as_str), Some("--version"), "{h}");
                    assert!(a.contains(&format!("{key}={value}")), "{h}: {a:?}");
                }
                (UpdatePolicy::Document { .. }, Ok(p)) => {
                    assert_eq!(args(&p.command).first().unwrap(), "--version", "{h}");
                    assert!(p._document.is_some(), "{h}: a document probe writes one");
                }
                (UpdatePolicy::Never { .. }, Ok(p)) => {
                    assert_eq!(args(&p.command), ["--version"], "{h}");
                    assert!(envs(&p.command).is_empty(), "{h}");
                }
                (UpdatePolicy::None { .. }, Err(ProbeError::Refused(r))) => {
                    assert!(matches!(r, ProbeRefusal::NoSwitch { .. }), "{h}: {r:?}");
                }
                (policy, got) => panic!("{h}: {policy:?} gave {got:?}"),
            }
        }
    }

    /// A pair row's flag is the row's own `Field::Pairs` spelling, after `--version`.
    #[test]
    fn a_pair_rides_the_rows_own_override_flag() {
        let spec = harness_spec(Harness::Codex);
        let flag = spec
            .argv
            .iter()
            .find_map(|a| match *a {
                Arg::Each(flag, Field::Pairs) => Some(flag),
                _ => None,
            })
            .expect("codex's row has an override channel");
        let UpdatePolicy::Pair { key, value, .. } = spec.updates else {
            panic!("codex's policy is a pair");
        };
        let p = version_probe(spec, "/bin/codex").unwrap();
        assert_eq!(
            args(&p.command),
            [
                "--version".into(),
                flag.to_string(),
                format!("{key}={value}"),
            ]
        );
    }

    /// A document row's probe names a private 0600 file in a 0700 directory, through the channel
    /// the row's declaration uses, carrying every key; the directory goes with the probe.
    #[test]
    fn a_document_is_private_carries_the_keys_and_is_removed_with_the_probe() {
        let spec = harness_spec(Harness::Gemini);
        let (
            Some(LiveDeclaration::EnvDocument { key: env_key, .. }),
            UpdatePolicy::Document { keys, .. },
        ) = (spec.live_declaration, spec.updates)
        else {
            panic!("gemini names its settings document by env and states document keys");
        };
        let p = version_probe(spec, "/bin/gemini").unwrap();
        let path = envs(&p.command)
            .into_iter()
            .find(|(k, _)| k == env_key)
            .map(|(_, v)| PathBuf::from(v))
            .expect("the document's path rides the row's own variable");
        let dir = p._document.as_ref().unwrap().path().to_path_buf();
        assert_eq!(path.parent(), Some(dir.as_path()));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        for (k, v) in keys {
            let ptr = format!("/{}", k.replace('.', "/"));
            assert_eq!(doc.pointer(&ptr), Some(&serde_json::Value::Bool(*v)), "{k}");
        }
        drop(p);
        assert!(!dir.exists(), "the probe's directory is removed with it");
    }
}
