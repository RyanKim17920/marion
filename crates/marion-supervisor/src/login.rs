//! `marion login` / `marion logout`: the user storing, listing and removing provider keys for
//! endpoint mode, and defining their own providers.
//!
//! **User-run only.** A key is read from a terminal with echo off, or — only when the user says so
//! on the command line — from stdin (`--stdin`) or from the provider's own environment variable
//! (`--from-env`). Without a terminal and without one of those flags the command refuses: an agent
//! or a script that ran `marion login` by accident must not sit waiting for, or silently take, a
//! key. Nothing here ever starts a vendor's own login flow.
//!
//! The key is never printed: every line this module writes names the provider and the store,
//! never the value.

use std::io::{self, BufRead, Read, Write};
use std::process::ExitCode;

use marion_core::provider::{self, AuthKind, CredentialId, ProviderDef, Registry, Wire};

use crate::credentials::{self, CredentialStore, Logins, Secret};

const USAGE: &str = "\
usage: marion login <provider>[:<label>] [--label <label>] [--from-env | --stdin]
       marion login --list
       marion login custom <id> --base-url <url> --wire <wire>[,<wire>] [--name <name>] [--auth api-key|none]
       marion logout <provider>[:<label>] [--label <label>]

  <provider>   a provider id; `marion login --list` shows them all
  --label      keep this key beside the provider's others (`openrouter:work`)
  --stdin      read the key from standard input instead of a terminal
  --from-env   import the key from the provider's own environment variable
  wires:       anthropic, openai-chat, openai-responses, gemini";

/// `marion login …` and `marion logout …`; `argv[0]` is the verb.
pub fn main(argv: &[String]) -> ExitCode {
    let mut out = io::stdout();
    let mut err = io::stderr();
    let code = match argv.first().map(String::as_str) {
        Some("logout") => logout(&argv[1..], &mut out),
        Some("login") => login(&argv[1..], &mut out),
        _ => Err(Failure::Usage),
    };
    match code {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage) => {
            let _ = writeln!(err, "{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Refused(why)) => {
            let _ = writeln!(err, "marion: {why}");
            ExitCode::FAILURE
        }
    }
}

enum Failure {
    Usage,
    Refused(String),
}

impl<E: std::fmt::Display> From<E> for Failure {
    fn from(e: E) -> Self {
        Failure::Refused(e.to_string())
    }
}

fn registry() -> Result<Registry, Failure> {
    credentials::user_registry().map_err(Failure::Refused)
}

fn store() -> Result<Box<dyn CredentialStore>, Failure> {
    Ok(credentials::default_store()?)
}

fn known<'r>(reg: &'r Registry, id: &str) -> Result<&'r ProviderDef, Failure> {
    reg.get(id).ok_or_else(|| {
        Failure::Refused(format!(
            "no provider named `{id}`; `marion login --list` shows the ones marion knows, and \
             `marion login custom {id} --base-url <url> --wire <wire>` defines your own"
        ))
    })
}

fn login(args: &[String], out: &mut dyn Write) -> Result<(), Failure> {
    match args.first().map(String::as_str) {
        Some("--list") if args.len() == 1 => list(out),
        Some("custom") => custom(&args[1..], out),
        Some(id) if !id.starts_with('-') => {
            let mut source = Source::Terminal;
            let mut label = None;
            let mut it = args[1..].iter();
            while let Some(flag) = it.next() {
                match flag.as_str() {
                    "--stdin" if source == Source::Terminal => source = Source::Stdin,
                    "--from-env" if source == Source::Terminal => source = Source::Env,
                    "--label" if label.is_none() => label = Some(it.next().ok_or(Failure::Usage)?),
                    _ => return Err(Failure::Usage),
                }
            }
            store_key(&credential_id(id, label)?, source, out)
        }
        _ => Err(Failure::Usage),
    }
}

/// `<provider>[:<label>]`, with `--label` as the other spelling of the same label.
fn credential_id(id: &str, label: Option<&String>) -> Result<CredentialId, Failure> {
    let spelled = match label {
        Some(l) if id.contains(':') => {
            return Err(Failure::Refused(format!(
                "`{id}` already names a label; drop `--label {l}` or the `:` part"
            )));
        }
        Some(l) => format!("{id}:{l}"),
        None => id.to_string(),
    };
    CredentialId::parse(&spelled).ok_or_else(|| {
        Failure::Refused(format!(
            "`{spelled}` is not a credential id: a provider id, optionally `:` and a label, each \
             lowercase letters, digits and `-`"
        ))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Terminal,
    Stdin,
    Env,
}

fn store_key(id: &CredentialId, source: Source, out: &mut dyn Write) -> Result<(), Failure> {
    let reg = registry()?;
    let p = known(&reg, &id.provider)?;
    if !p.auth.needs_credential() {
        writeln!(out, "{} needs no key; nothing stored.", p.name)?;
        return Ok(());
    }
    let key = match source {
        Source::Stdin => {
            let mut text = String::new();
            io::stdin().read_to_string(&mut text)?;
            Secret::new(&text)?
        }
        Source::Env => {
            let var = p.import_env.as_deref().ok_or_else(|| {
                Failure::Refused(format!("{} names no environment variable to import", p.id))
            })?;
            match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => Secret::new(&v)?,
                _ => return Err(Failure::Refused(format!("{var} is not set"))),
            }
        }
        Source::Terminal => read_from_terminal(p)?,
    };
    let store = store()?;
    store.put(&id.to_string(), &key)?;
    Logins::user()?.add(id)?;
    writeln!(out, "Stored a key for {id} in {}.", store.describe())?;
    Ok(())
}

/// Offer the provider's environment variable (with consent), else prompt with echo off. Refuses
/// outright when stdin is not a terminal.
fn read_from_terminal(p: &ProviderDef) -> Result<Secret, Failure> {
    let stdin = io::stdin();
    if !rustix::termios::isatty(&stdin) {
        return Err(Failure::Refused(format!(
            "`marion login {}` reads the key from a terminal, and stdin is not one. Pipe it with \
             `--stdin`, or import {} with `--from-env`",
            p.id,
            p.import_env.as_deref().unwrap_or("the provider's variable")
        )));
    }
    let mut err = io::stderr();
    if let Some(var) = p.import_env.as_deref()
        && std::env::var(var).is_ok_and(|v| !v.trim().is_empty())
    {
        write!(
            err,
            "{var} is set in your environment. Import it for {}? [y/N] ",
            p.id
        )?;
        err.flush()?;
        let mut answer = String::new();
        stdin.lock().read_line(&mut answer)?;
        if matches!(answer.trim(), "y" | "Y" | "yes") {
            return Ok(Secret::new(&std::env::var(var).unwrap_or_default())?);
        }
    }
    write!(err, "Paste your {} key (input hidden): ", p.name)?;
    err.flush()?;
    let line = read_hidden_line()?;
    writeln!(err)?;
    Ok(Secret::new(&line)?)
}

/// One line from the terminal on stdin with echo off, the terminal restored on every path.
fn read_hidden_line() -> io::Result<String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    let stdin = io::stdin();
    let saved = tcgetattr(&stdin)?;
    let mut quiet = saved.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    quiet.local_modes.insert(LocalModes::ECHONL);
    tcsetattr(&stdin, OptionalActions::Flush, &quiet)?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    let restored = tcsetattr(&stdin, OptionalActions::Flush, &saved);
    read?;
    restored?;
    Ok(line)
}

/// One provider as a listing shows it: who it is, and the credential ids stored for it. Never a
/// key: the ids are what `marion logout` takes, and [`CredentialStore::has`] is how they are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLogins {
    pub id: String,
    pub name: String,
    pub custom: bool,
    /// Its wires, comma-joined.
    pub wires: String,
    /// Whether it takes a key at all (a local server may not).
    pub needs_key: bool,
    /// Its stored credential ids, the unlabelled one first, then login order.
    pub stored: Vec<StoredId>,
}

/// A stored credential id, and why the store could not say, when it could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredId {
    pub id: String,
    pub unreadable: Option<String>,
}

/// Every provider `reg` knows, with the ids `store` holds for each: what `marion login --list`
/// prints and what the home screen's Setup lists.
pub fn stored_logins(
    reg: &Registry,
    store: &dyn CredentialStore,
    logins: &Logins,
) -> Vec<ProviderLogins> {
    reg.iter()
        .map(|p| {
            let needs_key = p.auth.needs_credential();
            ProviderLogins {
                id: p.id.clone(),
                name: p.name.clone(),
                custom: p.custom,
                wires: p.wire_list(),
                needs_key,
                stored: if needs_key {
                    stored_ids(store, logins, &p.id)
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}

/// [`stored_logins`] over the user's own registry, store and login index, and the store's name.
pub fn user_logins() -> Result<(Vec<ProviderLogins>, String), String> {
    let reg = credentials::user_registry()?;
    let store = credentials::default_store().map_err(|e| e.to_string())?;
    let logins = Logins::user().map_err(|e| e.to_string())?;
    Ok((
        stored_logins(&reg, store.as_ref(), &logins),
        store.describe(),
    ))
}

/// `marion login --list`: id, name, wires, and the credential ids stored for each provider in
/// login order. Never a key.
fn list(out: &mut dyn Write) -> Result<(), Failure> {
    let (providers, store) = user_logins().map_err(Failure::Refused)?;
    writeln!(
        out,
        "{:<16} {:<28} {:<40} KEYS",
        "PROVIDER", "NAME", "WIRES"
    )?;
    for p in providers {
        let stored = if !p.needs_key {
            "not needed".to_string()
        } else if p.stored.is_empty() {
            "-".to_string()
        } else {
            p.stored
                .iter()
                .map(|s| match &s.unreadable {
                    Some(e) => format!("{} (unreadable: {e})", s.id),
                    None => s.id.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let name = if p.custom {
            format!("{} (custom)", p.name)
        } else {
            p.name
        };
        writeln!(out, "{:<16} {:<28} {:<40} {stored}", p.id, name, p.wires)?;
    }
    writeln!(out, "\nkeys: {store}")?;
    Ok(())
}

/// The stored credential ids for `provider` in login order — the unlabelled one first where no
/// login index names it.
fn stored_ids(store: &dyn CredentialStore, logins: &Logins, provider: &str) -> Vec<StoredId> {
    let mut ids = logins.of(provider).unwrap_or_default();
    let default = CredentialId::default_for(provider);
    if !ids.contains(&default) {
        ids.insert(0, default);
    }
    ids.into_iter()
        .filter_map(|id| match store.has(&id.to_string()) {
            Ok(true) => Some(StoredId {
                id: id.to_string(),
                unreadable: None,
            }),
            Ok(false) => None,
            Err(e) => Some(StoredId {
                id: id.to_string(),
                unreadable: Some(e.to_string()),
            }),
        })
        .collect()
}

fn logout(args: &[String], out: &mut dyn Write) -> Result<(), Failure> {
    let id = match args {
        [id] => credential_id(id, None)?,
        [id, flag, label] if flag == "--label" => credential_id(id, Some(label))?,
        _ => return Err(Failure::Usage),
    };
    let reg = registry()?;
    known(&reg, &id.provider)?;
    let store = store()?;
    Logins::user()?.remove(&id)?;
    if store.delete(&id.to_string())? {
        writeln!(out, "Removed the key for {id} from {}.", store.describe())?;
    } else {
        writeln!(out, "No key was stored for {id}.")?;
    }
    Ok(())
}

/// `marion login custom <id> --base-url <url> --wire <w>[,<w>]`: add or replace a provider in the
/// user's `providers.toml`.
fn custom(args: &[String], out: &mut dyn Write) -> Result<(), Failure> {
    let Some((id, rest)) = args.split_first() else {
        return Err(Failure::Usage);
    };
    let (mut base_url, mut wires, mut name, mut auth) = (None, None, None, AuthKind::ApiKey);
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let value = it.next().ok_or(Failure::Usage)?;
        match flag.as_str() {
            "--base-url" => base_url = Some(value.clone()),
            "--name" => name = Some(value.clone()),
            "--wire" => {
                let mut ws = Vec::new();
                for w in value.split(',') {
                    ws.push(Wire::parse(w).ok_or_else(|| {
                        Failure::Refused(format!(
                            "unknown wire `{w}` (known: anthropic, openai-chat, openai-responses, \
                             gemini)"
                        ))
                    })?);
                }
                wires = Some(ws);
            }
            "--auth" => {
                auth = match value.as_str() {
                    "api-key" => AuthKind::ApiKey,
                    "none" => AuthKind::None,
                    _ => return Err(Failure::Usage),
                }
            }
            _ => return Err(Failure::Usage),
        }
    }
    let (Some(base_url), Some(wires)) = (base_url, wires) else {
        return Err(Failure::Usage);
    };
    let def = provider::custom_provider(id, &base_url, &wires, auth, name.as_deref(), None)?;
    let path = credentials::config_dir()?.join("providers.toml");
    let existing = credentials::registry_from(&path).map_err(Failure::Refused)?;
    let order = existing.credential_orders().clone();
    let mut defs: Vec<ProviderDef> = existing.custom().cloned().collect();
    let replaced = defs.iter().any(|d| d.id == def.id);
    defs.retain(|d| d.id != def.id);
    defs.push(def.clone());
    write_atomically(&path, &provider::render_custom(&defs, &order))?;
    writeln!(
        out,
        "{} provider {} ({}) in {}.",
        if replaced { "Replaced" } else { "Added" },
        def.id,
        def.wire_list(),
        path.display()
    )?;
    if def.auth.needs_credential() {
        writeln!(out, "Store its key with `marion login {}`.", def.id)?;
    }
    Ok(())
}

fn write_atomically(path: &std::path::Path, body: &str) -> io::Result<()> {
    let dir = path.parent().expect("a config file has a directory");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".providers.toml.{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::CredentialError;

    /// A store that answers whether a key is there and refuses to hand one over: a listing that
    /// reads a key to learn it exists fails here.
    struct NoReads(Vec<&'static str>);

    impl CredentialStore for NoReads {
        fn get(&self, _: &str) -> Result<Option<Secret>, CredentialError> {
            panic!("a listing read a key")
        }
        fn has(&self, id: &str) -> Result<bool, CredentialError> {
            Ok(self.0.contains(&id))
        }
        fn put(&self, _: &str, _: &Secret) -> Result<(), CredentialError> {
            unreachable!()
        }
        fn delete(&self, _: &str) -> Result<bool, CredentialError> {
            unreachable!()
        }
        fn describe(&self) -> String {
            "a test store".into()
        }
    }

    #[test]
    fn the_listing_groups_stored_ids_by_provider_without_reading_a_key() {
        let dir = std::env::temp_dir().join(format!("marion-login-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logins = Logins::at(dir.join("logins.json"));
        logins
            .add(&CredentialId::parse("openai:work").unwrap())
            .unwrap();
        let store = NoReads(vec!["openai", "openai:work"]);
        let listed = stored_logins(&Registry::seed(), &store, &logins);
        let openai = listed
            .iter()
            .find(|p| p.id == "openai")
            .expect("openai is listed");
        let ids: Vec<&str> = openai.stored.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["openai", "openai:work"],
            "unlabelled first, then login order"
        );
        assert!(
            listed
                .iter()
                .filter(|p| p.id != "openai")
                .all(|p| p.stored.is_empty())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
