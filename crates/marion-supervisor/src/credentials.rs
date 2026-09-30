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
//! (`marion_core::secret`, the one type every key and token marion holds is kept in) makes the
//! first of those structural: its `Debug` prints `***` and it has no `Display`, so a `{:?}` in an
//! error path cannot leak it. [`parse_key`] is the one way a typed or piped key becomes one.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use marion_core::provider::{CredentialId, Registry};

/// The environment variable that forces the file backend.
pub const STORE_ENV: &str = "MARION_CREDENTIAL_STORE";
/// The Keychain service every marion item is filed under; the account is the provider id.
pub const KEYCHAIN_SERVICE: &str = "marion";

pub use marion_core::secret::Secret;

/// A provider key, validated: non-empty, and only `[A-Za-z0-9._~+/=-]`. Every published key format
/// fits, and refusing everything else keeps a key from carrying a newline or quote into the
/// Keychain's command line or a harness's config document.
pub fn parse_key(key: &str) -> Result<Secret, CredentialError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(CredentialError::EmptyKey);
    }
    if !key.chars().all(valid_key_char) {
        return Err(CredentialError::BadKeyChars);
    }
    Ok(Secret::new(key))
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
    #[error(
        "`{0}` is not a credential id (`<provider>` or `<provider>:<label>`) marion can file a key under"
    )]
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
    /// Whether a key is stored under `provider`, for a listing: a backend that can answer without
    /// handing the key over (the Keychain's attribute lookup) does, so a listing never holds one.
    fn has(&self, provider: &str) -> Result<bool, CredentialError> {
        Ok(self.get(provider)?.is_some())
    }
    fn put(&self, provider: &str, key: &Secret) -> Result<(), CredentialError>;
    /// `true` when there was a key to remove.
    fn delete(&self, provider: &str) -> Result<bool, CredentialError>;
    /// One line naming the backend, for `marion login --list`.
    fn describe(&self) -> String;
}

/// A credential id as the stores accept it — `provider` or `provider:label`, each half the
/// registry's own id grammar ([`CredentialId`]), so a store key can never carry a character the
/// Keychain's command line or a JSON key would need to escape.
fn check_provider(id: &str) -> Result<(), CredentialError> {
    match CredentialId::parse(id) {
        Some(_) => Ok(()),
        None => Err(CredentialError::BadProvider(id.to_string())),
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

/// Whether [`default_store`] is the Keychain: macOS, and [`STORE_ENV`] does not say `file`.
pub fn keychain_in_use() -> bool {
    cfg!(target_os = "macos") && !std::env::var(STORE_ENV).is_ok_and(|v| v.trim() == "file")
}

/// The store `marion login` and a launch both use: the file backend where [`STORE_ENV`] says
/// `file` or the platform has no Keychain, the Keychain otherwise.
pub fn default_store() -> Result<Box<dyn CredentialStore>, CredentialError> {
    #[cfg(target_os = "macos")]
    if keychain_in_use() {
        return Ok(Box::new(Keychain::system()));
    }
    Ok(Box::new(FileStore::at(
        config_dir()?.join("credentials.json"),
    )))
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
        let mode = file
            .metadata()
            .map_err(|e| self.err(e))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(self.err(format!(
                "is readable by other users (mode {:o}); run `chmod 600` on it",
                mode & 0o777
            )));
        }
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(|e| self.err(e))?;
        let doc: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            self.err(format!(
                "is not valid JSON (line {}, column {})",
                e.line(),
                e.column()
            ))
        })?;
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
        use std::os::unix::fs::PermissionsExt;
        let dir = self
            .path
            .parent()
            .ok_or_else(|| self.err("has no parent directory"))?;
        crate::private_fs::create_dir_all(dir).map_err(|e| self.err(e))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| self.err(e))?;
        let doc = serde_json::json!({ "providers": keys });
        let body = serde_json::to_string_pretty(&doc).expect("a Value always serialises");
        write_private(&self.path, &body).map_err(|e| self.err(e))
    }
}

/// Replace `path` with `body` and a final newline, `0600`: see [`crate::private_fs::write_atomic`].
fn write_private(path: &Path, body: &str) -> io::Result<()> {
    crate::private_fs::write_atomic(path, format!("{body}\n").as_bytes())
}

/// **Which credentials the user has logged in, in login order** — the non-secret index beside
/// the store (`logins.json`: ids only, never a key). The Keychain cannot be listed without reading
/// every item, so this is what `marion login --list` groups and what a launch falls back to for
/// the order to try a provider's credentials in when the user stated none.
pub struct Logins {
    path: PathBuf,
}

impl Logins {
    pub fn user() -> Result<Self, CredentialError> {
        Ok(Logins::at(config_dir()?.join("logins.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        Logins { path }
    }

    fn err(&self, why: impl std::fmt::Display) -> CredentialError {
        CredentialError::File {
            path: self.path.clone(),
            why: why.to_string(),
        }
    }

    /// Every logged-in credential, in login order. An absent index is none.
    pub fn all(&self) -> Result<Vec<CredentialId>, CredentialError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.err(e)),
        };
        let doc: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| self.err(format!("is not valid JSON (line {})", e.line())))?;
        Ok(doc
            .get("logins")
            .and_then(|l| l.as_array())
            .map(|ids| {
                ids.iter()
                    .filter_map(|v| v.as_str().and_then(CredentialId::parse))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `provider`'s logged-in credentials, in login order.
    pub fn of(&self, provider: &str) -> Result<Vec<CredentialId>, CredentialError> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|c| c.provider == provider)
            .collect())
    }

    fn save(&self, ids: &[CredentialId]) -> Result<(), CredentialError> {
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let body = serde_json::to_string_pretty(&serde_json::json!({ "logins": ids }))
            .expect("a Value always serialises");
        write_private(&self.path, &body).map_err(|e| self.err(e))
    }

    /// Note `id` as logged in (at the end, if it is new).
    pub fn add(&self, id: &CredentialId) -> Result<(), CredentialError> {
        let mut ids = self.all()?;
        if !ids.contains(id) {
            ids.push(id.clone());
            self.save(&ids)?;
        }
        Ok(())
    }

    /// Forget `id`.
    pub fn remove(&self, id: &CredentialId) -> Result<(), CredentialError> {
        let mut ids = self.all()?;
        let before = ids.len();
        ids.retain(|c| c != id);
        if ids.len() != before {
            self.save(&ids)?;
        }
        Ok(())
    }
}

impl CredentialStore for FileStore {
    fn get(&self, provider: &str) -> Result<Option<Secret>, CredentialError> {
        check_provider(provider)?;
        match self.load()?.get(provider) {
            Some(k) => parse_key(k).map(Some),
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

/// **The macOS login Keychain, read and written in-process** through Security.framework: service
/// [`KEYCHAIN_SERVICE`], account the credential id.
///
/// In-process because the Keychain trusts the *application* that creates an item. Created by
/// `/usr/bin/security`, an item trusted that tool, and any process of the operator's — a model's
/// shell included — could run `security find-generic-password -w` and read the key without a
/// prompt. Created by marion, it trusts marion: marion reads silently and anything else is asked.
/// An unsigned build is identified by its code hash, so the first read after an upgrade shows one
/// "marion wants to access" dialog (Always Allow ends it); a Developer-ID-signed release keeps the
/// identity across upgrades.
///
/// [`Keychain::put`] deletes before it adds, so a re-stored item is created by marion and trusts
/// it alone, and records the id in [`KEYCHAIN_OWNED_FILE`]: an id the store holds that is not
/// recorded there was stored by the `security` tool, and `marion doctor` names it
/// ([`unowned_keychain_ids`]) with the command that re-stores it.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct Keychain {
    /// Where the ids marion itself created are recorded; `None` records nothing (a test's).
    owned: Option<PathBuf>,
}

/// The ids whose Keychain items marion created itself, under the user-level config dir (ids only,
/// never a key).
pub const KEYCHAIN_OWNED_FILE: &str = "keychain-owned.json";

/// `errSecItemNotFound`.
#[cfg(target_os = "macos")]
const ERR_ITEM_NOT_FOUND: i32 = -25300;

#[cfg(target_os = "macos")]
impl Keychain {
    pub fn system() -> Self {
        Keychain {
            owned: config_dir().ok().map(|d| d.join(KEYCHAIN_OWNED_FILE)),
        }
    }

    fn err(what: &str, e: security_framework::base::Error) -> CredentialError {
        CredentialError::Keychain(format!("{what}: {e}"))
    }

    fn record(&self, id: &str, owned: bool) -> Result<(), CredentialError> {
        let Some(path) = &self.owned else {
            return Ok(());
        };
        let mut ids = read_owned(path)?;
        ids.retain(|i| i != id);
        if owned {
            ids.push(id.to_string());
        }
        let doc = serde_json::json!({ "owned": ids });
        crate::private_fs::write_atomic(path, format!("{doc:#}\n").as_bytes())
            .map_err(|e| CredentialError::Keychain(format!("{}: {e}", path.display())))
    }
}

/// The ids recorded in `path`; an absent file is none.
fn read_owned(path: &Path) -> Result<Vec<String>, CredentialError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(CredentialError::Keychain(format!(
                "{}: {e}",
                path.display()
            )));
        }
    };
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| CredentialError::Keychain(format!("{}: {e}", path.display())))?;
    Ok(doc["owned"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect())
}

/// The credential ids `logins` lists whose Keychain item marion did not create itself — stored by
/// the `security` tool before marion wrote its own items, so still readable by any process of the
/// operator's without a prompt. Empty wherever the file store is in use.
pub fn unowned_keychain_ids(
    logins: &[CredentialId],
    owned_file: &Path,
) -> Result<Vec<String>, CredentialError> {
    let owned = read_owned(owned_file)?;
    Ok(logins
        .iter()
        .map(|id| id.to_string())
        .filter(|id| !owned.contains(id))
        .collect())
}

/// The command that re-stores `id` as marion's own item: read through the tool the old item
/// trusts, stored in-process.
pub fn restore_command(id: &str) -> String {
    format!(
        "security find-generic-password -s {KEYCHAIN_SERVICE} -a {id} -w | marion login {id} --stdin"
    )
}

#[cfg(target_os = "macos")]
impl CredentialStore for Keychain {
    fn get(&self, provider: &str) -> Result<Option<Secret>, CredentialError> {
        check_provider(provider)?;
        match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, provider) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|_| CredentialError::Keychain("the item is not UTF-8".into()))?;
                parse_key(&text).map(Some)
            }
            Err(e) if e.code() == ERR_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(Self::err("reading the item", e)),
        }
    }

    /// The item's attributes only, never its password: no access check, so no prompt.
    fn has(&self, provider: &str) -> Result<bool, CredentialError> {
        check_provider(provider)?;
        let found = security_framework::item::ItemSearchOptions::new()
            .class(security_framework::item::ItemClass::generic_password())
            .service(KEYCHAIN_SERVICE)
            .account(provider)
            .load_attributes(true)
            .search();
        match found {
            Ok(items) => Ok(!items.is_empty()),
            Err(e) if e.code() == ERR_ITEM_NOT_FOUND => Ok(false),
            Err(e) => Err(Self::err("looking the item up", e)),
        }
    }

    fn put(&self, provider: &str, key: &Secret) -> Result<(), CredentialError> {
        check_provider(provider)?;
        // Delete first, so the item is created by marion and trusts marion alone: an update would
        // keep whatever access list the old item had.
        match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, provider) {
            Ok(()) => {}
            Err(e) if e.code() == ERR_ITEM_NOT_FOUND => {}
            Err(e) => return Err(Self::err("replacing the old item", e)),
        }
        security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            provider,
            key.expose().as_bytes(),
        )
        .map_err(|e| Self::err("storing the item", e))?;
        self.record(provider, true)
    }

    fn delete(&self, provider: &str) -> Result<bool, CredentialError> {
        check_provider(provider)?;
        let gone = match security_framework::passwords::delete_generic_password(
            KEYCHAIN_SERVICE,
            provider,
        ) {
            Ok(()) => true,
            Err(e) if e.code() == ERR_ITEM_NOT_FOUND => false,
            Err(e) => return Err(Self::err("deleting the item", e)),
        };
        self.record(provider, false)?;
        Ok(gone)
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
        let s = parse_key("sk-very-secret-123").unwrap();
        let shown = format!("{s:?} {:?}", Some(&s));
        assert!(!shown.contains("very-secret"), "{shown}");
        assert!(shown.contains("***"));
    }

    #[test]
    fn a_key_with_a_character_no_provider_uses_is_refused() {
        for bad in ["", "   ", "sk key", "sk\nkey", "sk\"key", "sk;rm", "sk'k"] {
            assert!(parse_key(bad).is_err(), "{bad:?}");
        }
        for good in ["sk-proj-AbC_123", "abc.def~g+h/i=j", "  sk-trim  "] {
            assert!(parse_key(good).is_ok(), "{good:?}");
        }
        assert_eq!(parse_key("  sk-trim ").unwrap().expose(), "sk-trim");
    }

    #[test]
    fn the_file_store_round_trips_at_0600_in_a_0700_dir() {
        let dir = tmp("roundtrip").join("marion");
        let store = FileStore::at(dir.join("credentials.json"));
        assert!(store.get("openai").unwrap().is_none());
        store.put("openai", &parse_key("sk-one").unwrap()).unwrap();
        store.put("groq", &parse_key("gsk-two").unwrap()).unwrap();
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
            assert!(store.put(bad, &parse_key("k").unwrap()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_labelled_credential_is_its_own_entry_beside_the_default() {
        let store = FileStore::at(tmp("labels").join("c.json"));
        store
            .put("openrouter", &parse_key("sk-default").unwrap())
            .unwrap();
        store
            .put("openrouter:work", &parse_key("sk-work").unwrap())
            .unwrap();
        assert_eq!(
            store.get("openrouter").unwrap().unwrap().expose(),
            "sk-default"
        );
        assert_eq!(
            store.get("openrouter:work").unwrap().unwrap().expose(),
            "sk-work"
        );
        assert!(store.get("openrouter:personal").unwrap().is_none());
        assert!(store.get("openrouter:Work").is_err());
    }

    #[test]
    fn the_logins_index_keeps_login_order_and_holds_no_key() {
        let dir = tmp("logins");
        let logins = Logins::at(dir.join("logins.json"));
        assert!(logins.all().unwrap().is_empty());
        let id = |s: &str| CredentialId::parse(s).unwrap();
        for s in ["openrouter:work", "groq", "openrouter", "openrouter:work"] {
            logins.add(&id(s)).unwrap();
        }
        assert_eq!(
            logins.of("openrouter").unwrap(),
            vec![id("openrouter:work"), id("openrouter")]
        );
        logins.remove(&id("openrouter:work")).unwrap();
        assert_eq!(logins.of("openrouter").unwrap(), vec![id("openrouter")]);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("logins.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_registry_file_that_is_absent_is_the_seed_table() {
        let reg = registry_from(&tmp("noreg").join("providers.toml")).unwrap();
        assert!(reg.get("openai").is_some());
        assert_eq!(reg.custom().count(), 0);
    }

    /// An id the store lists that marion did not create itself is named, with the command that
    /// re-stores it as marion's own item; ids marion created are not.
    #[test]
    fn an_item_the_security_tool_stored_is_named_with_its_restore_command() {
        let dir = tmp("keychain-owned");
        std::fs::create_dir_all(&dir).unwrap();
        let owned = dir.join(KEYCHAIN_OWNED_FILE);
        let ids: Vec<CredentialId> = ["openrouter", "openai:work"]
            .iter()
            .map(|i| CredentialId::parse(i).unwrap())
            .collect();
        assert_eq!(
            unowned_keychain_ids(&ids, &owned).unwrap(),
            ["openrouter", "openai:work"]
        );
        std::fs::write(&owned, r#"{"owned":["openrouter"]}"#).unwrap();
        assert_eq!(unowned_keychain_ids(&ids, &owned).unwrap(), ["openai:work"]);
        assert_eq!(
            restore_command("openai:work"),
            "security find-generic-password -s marion -a openai:work -w | marion login openai:work \
             --stdin"
        );
    }

    /// Touches the real login Keychain, so it runs only when asked for by name.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_keychain_round_trips_when_explicitly_enabled() {
        if std::env::var("MARION_KEYCHAIN_TEST").as_deref() != Ok("1") {
            eprintln!("skipped: set MARION_KEYCHAIN_TEST=1 to exercise the macOS Keychain");
            return;
        }
        let kc = Keychain { owned: None };
        let id = format!("marion-keychain-test-{}", std::process::id());
        let key = parse_key("sk-keychain-test-value").unwrap();
        kc.put(&id, &key).unwrap();
        assert_eq!(kc.get(&id).unwrap(), Some(key));
        assert!(kc.has(&id).unwrap(), "the attribute lookup finds it");
        assert!(kc.delete(&id).unwrap());
        assert_eq!(kc.get(&id).unwrap(), None);
        assert!(!kc.has(&id).unwrap());
        assert!(!kc.delete(&id).unwrap());
    }
}
