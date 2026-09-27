//! The user's own marion configuration: **provider keys** (`marion login`) and **custom
//! providers** (`providers.toml`).
//!
//! Both live at user level — `$XDG_CONFIG_HOME/marion/`, else `~/.config/marion/` — and never in a
//! repository: a `.marion/providers.toml` checked into a project would let whoever wrote it point
//! a node, and the key marion presents for it, at an endpoint of their choosing. Nothing here reads
//! a project directory.
//!
//! # Where a key is kept
//!
//! On macOS the login Keychain, through `/usr/bin/security`; elsewhere, or when
//! `MARION_CREDENTIAL_STORE=file`, a `0600` JSON file in a `0700` directory. Writes to the Keychain
//! go through `security -i` with the command on **stdin**, so the key is never on an argv another
//! user's `ps` can read.
//!
//! # What a key never does
//!
//! It is never printed, journaled, written into a contract or left in captured output. [`Secret`]
//! is the type that makes the first of those structural: its `Debug` prints `***` and it has no
//! `Display`, so a `{:?}` in an error path cannot leak it.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use marion_core::provider::Registry;

/// The environment variable that forces the file backend.
pub const STORE_ENV: &str = "MARION_CREDENTIAL_STORE";
/// The Keychain service every marion item is filed under; the account is the provider id.
pub const KEYCHAIN_SERVICE: &str = "marion";

/// A provider key. `Debug` prints `***`; there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// A key, validated: non-empty, and only `[A-Za-z0-9._~+/=-]`. Every published key format
    /// fits, and refusing everything else keeps a key from carrying a newline or quote into the
    /// Keychain's command line or a harness's config document.
    pub fn new(key: &str) -> Result<Self, CredentialError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(CredentialError::EmptyKey);
        }
        if !key.chars().all(valid_key_char) {
            return Err(CredentialError::BadKeyChars);
        }
        Ok(Secret(key.to_string()))
    }

    /// The key itself, for the one place that hands it to a harness.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

fn valid_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '+' | '/' | '=' | '-')
}

/// Why a credential operation failed. No variant carries a key.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("the key is empty")]
    EmptyKey,
    #[error(
        "the key contains characters no provider key uses (allowed: letters, digits and . _ ~ + / \
         = -); check what was pasted"
    )]
    BadKeyChars,
    #[error("`{0}` is not a provider id marion can file a key under")]
    BadProvider(String),
    #[error("{path}: {why}")]
    File { path: PathBuf, why: String },
    #[error("the macOS Keychain refused ({0}); set {STORE_ENV}=file to use a file instead")]
    Keychain(String),
    #[error("no config directory: neither XDG_CONFIG_HOME nor HOME is set")]
    NoConfigDir,
}

/// Where provider keys are kept.
pub trait CredentialStore {
    fn get(&self, provider: &str) -> Result<Option<Secret>, CredentialError>;
    fn put(&self, provider: &str, key: &Secret) -> Result<(), CredentialError>;
    /// `true` when there was a key to remove.
    fn delete(&self, provider: &str) -> Result<bool, CredentialError>;
    /// One line naming the backend, for `marion login --list`.
    fn describe(&self) -> String;
}

/// A provider id as the stores accept it — the registry's own id grammar, so a store key can never
/// carry a character the Keychain's command line or a JSON key would need to escape.
fn check_provider(provider: &str) -> Result<(), CredentialError> {
    if marion_core::provider::valid_id(provider) {
        Ok(())
    } else {
        Err(CredentialError::BadProvider(provider.to_string()))
    }
}

/// `$XDG_CONFIG_HOME/marion`, else `$HOME/.config/marion`. An empty or relative
/// `XDG_CONFIG_HOME` is ignored, as the XDG spec says.
pub fn config_dir() -> Result<PathBuf, CredentialError> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && x.is_absolute()
    {
        return Ok(x.join("marion"));
    }
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(h) if h.is_absolute() => Ok(h.join(".config").join("marion")),
        _ => Err(CredentialError::NoConfigDir),
    }
}

/// The store `marion login` and a launch both use: the file backend where [`STORE_ENV`] says
/// `file` or the platform has no Keychain, the Keychain otherwise.
pub fn default_store() -> Result<Box<dyn CredentialStore>, CredentialError> {
    let forced_file = std::env::var(STORE_ENV).is_ok_and(|v| v.trim() == "file");
    if cfg!(target_os = "macos") && !forced_file {
        return Ok(Box::new(Keychain::system()));
    }
    Ok(Box::new(FileStore::at(config_dir()?.join("credentials.json"))))
}

/// The seed providers plus the user's `providers.toml`, if there is one. **Only** the user-level
/// file: no project directory is consulted.
pub fn user_registry() -> Result<Registry, String> {
    let dir = config_dir().map_err(|e| e.to_string())?;
    registry_from(&dir.join("providers.toml"))
}

/// [`user_registry`] for an explicit file — absent is the seed table alone.
pub fn registry_from(path: &Path) -> Result<Registry, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Registry::with_custom(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Registry::seed()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The file backend: `credentials.json`, `{"providers": {"<id>": "<key>"}}`, mode `0600` in a
/// `0700` directory, replaced atomically on every write.
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn at(path: PathBuf) -> Self {
        FileStore { path }
    }

    fn err(&self, why: impl std::fmt::Display) -> CredentialError {
        CredentialError::File {
            path: self.path.clone(),
            why: why.to_string(),
        }
    }

    fn load(&self) -> Result<BTreeMap<String, String>, CredentialError> {
        use std::os::unix::fs::PermissionsExt;
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(self.err(e)),
        };
        // A key file others can read is refused rather than used, as ssh refuses such a key.
        let mode = file.metadata().map_err(|e| self.err(e))?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(self.err(format!(
                "is readable by other users (mode {:o}); run `chmod 600` on it",
                mode & 0o777
            )));
        }
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(|e| self.err(e))?;
        let doc: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| self.err(format!("is not valid JSON (line {}, column {})", e.line(), e.column())))?;
        let mut out = BTreeMap::new();
        if let Some(map) = doc.get("providers").and_then(|p| p.as_object()) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }
        Ok(out)
    }

    fn save(&self, keys: &BTreeMap<String, String>) -> Result<(), CredentialError> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        let dir = self
            .path
            .parent()
            .ok_or_else(|| self.err("has no parent directory"))?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| self.err(e))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| self.err(e))?;
        let doc = serde_json::json!({ "providers": keys });
        let body = serde_json::to_string_pretty(&doc).expect("a Value always serialises");
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).map_err(|e| self.err(e))?;
        let tmp = dir.join(format!(
            ".credentials.json.{}.{}",
            std::process::id(),
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        let written = (|| -> io::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(self.err(e));
        }
        Ok(())
    }
}

impl CredentialStore for FileStore {
    fn get(&self, provider: &str) -> Result<Option<Secret>, CredentialError> {
        check_provider(provider)?;
        match self.load()?.get(provider) {
            Some(k) => Secret::new(k).map(Some),
            None => Ok(None),
        }
    }

    fn put(&self, provider: &str, key: &Secret) -> Result<(), CredentialError> {
        check_provider(provider)?;
        let mut keys = self.load()?;
        keys.insert(provider.to_string(), key.expose().to_string());
        self.save(&keys)
    }

    fn delete(&self, provider: &str) -> Result<bool, CredentialError> {
        check_provider(provider)?;
        let mut keys = self.load()?;
        let had = keys.remove(provider).is_some();
        if had {
            self.save(&keys)?;
        }
        Ok(had)
    }

    fn describe(&self) -> String {
        format!("file {}", self.path.display())
    }
}

/// The macOS login Keychain, through `/usr/bin/security`: service [`KEYCHAIN_SERVICE`], account
/// the provider id.
#[derive(Debug, Clone)]
pub struct Keychain {
    program: PathBuf,
}

/// `security`'s exit status for "the specified item could not be found".
const ERR_ITEM_NOT_FOUND: i32 = 44;

impl Keychain {
    pub fn system() -> Self {
        Keychain {
            program: PathBuf::from("/usr/bin/security"),
        }
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<(i32, String), CredentialError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            // Never shown: on `-i` it could echo the command line back.
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| CredentialError::Keychain(format!("cannot run security: {e}")))?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("piped");
            pipe.write_all(input.as_bytes())
                .map_err(|e| CredentialError::Keychain(e.to_string()))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| CredentialError::Keychain(e.to_string()))?;
        let code = out.status.code().unwrap_or(-1);
        Ok((code, String::from_utf8_lossy(&out.stdout).into_owned()))
    }
}

impl CredentialStore for Keychain {
    fn get(&self, provider: &str) -> Result<Option<Secret>, CredentialError> {
        check_provider(provider)?;
        let (code, out) = self.run(
            &[
                "find-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                provider,
                "-w",
            ],
            None,
        )?;
        match code {
            0 => Secret::new(out.trim_end_matches('\n')).map(Some),
            ERR_ITEM_NOT_FOUND => Ok(None),
            c => Err(CredentialError::Keychain(format!(
                "find-generic-password exited {c}"
            ))),
        }
    }

    fn put(&self, provider: &str, key: &Secret) -> Result<(), CredentialError> {
        check_provider(provider)?;
        // Both tokens are validated to a character set `security -i`'s tokenizer passes through
        // unquoted, and the line travels on stdin, never argv.
        let line = format!(
            "add-generic-password -U -s {KEYCHAIN_SERVICE} -a {provider} -w {}\n",
            key.expose()
        );
        let (code, _) = self.run(&["-i"], Some(&line))?;
        if code != 0 {
            return Err(CredentialError::Keychain(format!(
                "add-generic-password exited {code}"
            )));
        }
        // `-i` exits 0 even when the command inside it failed; read the item back to know.
        match self.get(provider)? {
            Some(k) if k == *key => Ok(()),
            _ => Err(CredentialError::Keychain(
                "the item was not stored".to_string(),
            )),
        }
    }

    fn delete(&self, provider: &str) -> Result<bool, CredentialError> {
        check_provider(provider)?;
        let (code, _) = self.run(
            &[
                "delete-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                provider,
            ],
            None,
        )?;
        match code {
            0 => Ok(true),
            ERR_ITEM_NOT_FOUND => Ok(false),
            c => Err(CredentialError::Keychain(format!(
                "delete-generic-password exited {c}"
            ))),
        }
    }

    fn describe(&self) -> String {
        format!("macOS Keychain (service \"{KEYCHAIN_SERVICE}\")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marion-cred-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_secret_debugs_as_stars_and_never_as_itself() {
        let s = Secret::new("sk-very-secret-123").unwrap();
        let shown = format!("{s:?} {:?}", Some(&s));
        assert!(!shown.contains("very-secret"), "{shown}");
        assert!(shown.contains("***"));
    }

    #[test]
    fn a_key_with_a_character_no_provider_uses_is_refused() {
        for bad in ["", "   ", "sk key", "sk\nkey", "sk\"key", "sk;rm", "sk'k"] {
            assert!(Secret::new(bad).is_err(), "{bad:?}");
        }
        for good in ["sk-proj-AbC_123", "abc.def~g+h/i=j", "  sk-trim  "] {
            assert!(Secret::new(good).is_ok(), "{good:?}");
        }
        assert_eq!(Secret::new("  sk-trim ").unwrap().expose(), "sk-trim");
    }

    #[test]
    fn the_file_store_round_trips_at_0600_in_a_0700_dir() {
        let dir = tmp("roundtrip").join("marion");
        let store = FileStore::at(dir.join("credentials.json"));
        assert!(store.get("openai").unwrap().is_none());
        store.put("openai", &Secret::new("sk-one").unwrap()).unwrap();
        store.put("groq", &Secret::new("gsk-two").unwrap()).unwrap();
        assert_eq!(store.get("openai").unwrap().unwrap().expose(), "sk-one");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("credentials.json")), 0o600);
        assert_eq!(mode(&dir), 0o700);
        // No temp file is left beside it.
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("credentials.json")]);
        assert!(store.delete("openai").unwrap());
        assert!(!store.delete("openai").unwrap());
        assert!(store.get("openai").unwrap().is_none());
        assert_eq!(store.get("groq").unwrap().unwrap().expose(), "gsk-two");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_key_file_other_users_can_read_is_refused_and_the_error_carries_no_key() {
        let dir = tmp("wide");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(&path, r#"{"providers":{"openai":"sk-leaky"}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = FileStore::at(path).get("openai").unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        assert!(!err.contains("sk-leaky"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_provider_id_outside_the_registry_grammar_is_refused_by_every_store() {
        let store = FileStore::at(tmp("badid").join("c.json"));
        for bad in ["", "Open AI", "a;b", "-x", "a b"] {
            assert!(store.get(bad).is_err(), "{bad:?}");
            assert!(store.put(bad, &Secret::new("k").unwrap()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_registry_file_that_is_absent_is_the_seed_table() {
        let reg = registry_from(&tmp("noreg").join("providers.toml")).unwrap();
        assert!(reg.get("openai").is_some());
        assert_eq!(reg.custom().count(), 0);
    }

    /// Touches the real login Keychain, so it runs only when asked for by name.
    #[test]
    fn the_keychain_round_trips_when_explicitly_enabled() {
        if std::env::var("MARION_KEYCHAIN_TEST").as_deref() != Ok("1") {
            eprintln!("skipped: set MARION_KEYCHAIN_TEST=1 to exercise the macOS Keychain");
            return;
        }
        let kc = Keychain::system();
        let id = format!("marion-keychain-test-{}", std::process::id());
        let key = Secret::new("sk-keychain-test-value").unwrap();
        kc.put(&id, &key).unwrap();
        assert_eq!(kc.get(&id).unwrap(), Some(key));
        assert!(kc.delete(&id).unwrap());
        assert_eq!(kc.get(&id).unwrap(), None);
        assert!(!kc.delete(&id).unwrap());
    }
}
