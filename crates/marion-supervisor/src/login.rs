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

use marion_core::provider::{self, AuthKind, ProviderDef, Registry, Wire};

use crate::credentials::{self, CredentialStore, Secret};

const USAGE: &str = "\
usage: marion login <provider> [--from-env | --stdin]
       marion login --list
       marion login custom <id> --base-url <url> --wire <wire>[,<wire>] [--name <name>] [--auth api-key|none]
       marion logout <provider>

  <provider>   a provider id; `marion login --list` shows them all
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
            let source = match &args[1..] {
                [] => Source::Terminal,
                [f] if f == "--stdin" => Source::Stdin,
                [f] if f == "--from-env" => Source::Env,
                _ => return Err(Failure::Usage),
            };
            store_key(id, source, out)
        }
        _ => Err(Failure::Usage),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Terminal,
    Stdin,
    Env,
}

fn store_key(id: &str, source: Source, out: &mut dyn Write) -> Result<(), Failure> {
    let reg = registry()?;
    let p = known(&reg, id)?;
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
    store.put(&p.id, &key)?;
    writeln!(out, "Stored a key for {} in {}.", p.id, store.describe())?;
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

/// `marion login --list`: id, name, wires, whether a key is stored. Never the key.
fn list(out: &mut dyn Write) -> Result<(), Failure> {
    let reg = registry()?;
    let store = store()?;
    writeln!(out, "{:<16} {:<28} {:<40} KEY", "PROVIDER", "NAME", "WIRES")?;
    for p in reg.iter() {
        let stored = if !p.auth.needs_credential() {
            "not needed".to_string()
        } else {
            match store.get(&p.id) {
                Ok(Some(_)) => "stored".to_string(),
                Ok(None) => "-".to_string(),
                Err(e) => format!("unreadable: {e}"),
            }
        };
        let name = if p.custom {
            format!("{} (custom)", p.name)
        } else {
            p.name.clone()
        };
        writeln!(
            out,
            "{:<16} {:<28} {:<40} {stored}",
            p.id,
            name,
            p.wire_list()
        )?;
    }
    writeln!(out, "\nkeys: {}", store.describe())?;
    Ok(())
}

fn logout(args: &[String], out: &mut dyn Write) -> Result<(), Failure> {
    let [id] = args else {
        return Err(Failure::Usage);
    };
    let reg = registry()?;
    let p = known(&reg, id)?;
    let store = store()?;
    if store.delete(&p.id)? {
        writeln!(
            out,
            "Removed the key for {} from {}.",
            p.id,
            store.describe()
        )?;
    } else {
        writeln!(out, "No key was stored for {}.", p.id)?;
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
