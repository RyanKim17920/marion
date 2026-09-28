//! **`marion profile`** — add, list, choose and remove the operator's own logins.
//!
//! ```text
//! marion profile add <harness> <name> [--dir <path>]
//! marion profile list [--state-dir <path>]
//! marion profile use <harness> <name>
//! marion profile remove <name> [--purge]
//! ```
//!
//! `add` creates the profile's directory, links the harness's non-secret shared files into it,
//! records it in `profiles.toml`, and **prints** the one command that logs in — marion never runs
//! it, never opens a browser, and never touches a credential. `list` asks each harness's own
//! read-only status probe and shows the last usage reading children's streams left, with its age;
//! it makes no network call of marion's own. `use` sets the harness's default. `remove` forgets a
//! profile, and with `--purge` deletes the directory marion created for it.
//!
//! Every function takes its paths and writes to a caller's `Write`, so the verbs are tested without
//! a terminal or a real home.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use marion_core::harness::Harness;

use crate::profiles::{
    Listed, LoginState, ProfileEntry, ProfileError, ProfilePaths, ProfilesFile, carrier,
    display_name, listed, login_state, parse_harness,
};

/// The usage line `marion --help` shows, and what a malformed `profile` verb prints.
pub const USAGE: &str = "usage: marion profile add <harness> <name> [--dir <path>]\n\
     \x20      marion profile list [--state-dir <path>]\n\
     \x20      marion profile use <harness> <name>\n\
     \x20      marion profile remove <name> [--purge]";

/// `marion profile …`, from this process's environment. `argv` is everything after `profile`.
pub fn main(argv: &[String]) -> ExitCode {
    let Some(paths) = ProfilePaths::from_env() else {
        eprintln!(
            "marion profile: none of $XDG_CONFIG_HOME, $XDG_DATA_HOME or $HOME is set, so there is \
             nowhere to keep profiles"
        );
        return ExitCode::FAILURE;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut out = std::io::stdout();
    match run(argv, &paths, home.as_deref(), &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Refusal::Usage) => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(Refusal::Profile(e)) => {
            eprintln!("marion profile: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Why a verb did nothing.
#[derive(Debug)]
pub enum Refusal {
    Usage,
    Profile(ProfileError),
}

impl From<ProfileError> for Refusal {
    fn from(e: ProfileError) -> Self {
        Refusal::Profile(e)
    }
}

impl From<std::io::Error> for Refusal {
    fn from(e: std::io::Error) -> Self {
        Refusal::Profile(ProfileError::Io(e.to_string()))
    }
}

/// One verb, against `paths`, printing to `out`. `home` is where a harness's default directory
/// lives, for the shared files `add` links.
pub fn run(
    argv: &[String],
    paths: &ProfilePaths,
    home: Option<&Path>,
    out: &mut dyn Write,
) -> Result<(), Refusal> {
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["add", harness, name, rest @ ..] => {
            let dir = match rest {
                [] => None,
                ["--dir", dir] => Some(*dir),
                _ => return Err(Refusal::Usage),
            };
            add(paths, home, harness, name, dir, out)
        }
        ["list"] => list(paths, out),
        ["list", "--state-dir", state] => {
            let paths = paths.clone().with_state_root(Path::new(state));
            list(&paths, out)
        }
        ["use", harness, name] => use_default(paths, harness, name, out),
        ["remove", name] => remove(paths, name, false, out),
        ["remove", name, "--purge"] => remove(paths, name, true, out),
        _ => Err(Refusal::Usage),
    }
}

fn harness_named(word: &str) -> Result<Harness, Refusal> {
    parse_harness(word).ok_or_else(|| {
        Refusal::Profile(ProfileError::Config {
            path: "marion profile".into(),
            why: format!("unknown harness {word:?}"),
        })
    })
}

/// **`add`**: the directory, the shared links, the entry — and the login command, printed.
fn add(
    paths: &ProfilePaths,
    home: Option<&Path>,
    harness: &str,
    name: &str,
    dir: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), Refusal> {
    let harness = harness_named(harness)?;
    let carrier = carrier(harness)?;
    if !marion_core::agent_type::is_valid_name(name) {
        return Err(ProfileError::InvalidName(name.into()).into());
    }
    let mut file = ProfilesFile::load(&paths.config)?;
    if file.find(name).is_some() {
        return Err(ProfileError::Duplicate(name.into()).into());
    }
    // Stored exactly as it will be exported: an adopted directory as the operator typed it, a
    // new one as marion made it.
    let dir = dir
        .map(str::to_string)
        .unwrap_or_else(|| paths.dir_for(harness, name).to_string_lossy().into_owned());
    std::fs::create_dir_all(&dir)?;
    let shared = link_shared(home, carrier, Path::new(&dir))?;
    file.profile.push(ProfileEntry {
        name: name.into(),
        harness: harness.as_str().into(),
        dir: dir.clone(),
    });
    file.save(&paths.config)?;
    writeln!(out, "profile `{name}` ({}) is {dir}", display_name(harness))?;
    if !shared.is_empty() {
        writeln!(
            out,
            "shared from ~/{}: {}",
            carrier.home_default,
            shared.join(", ")
        )?;
    }
    writeln!(
        out,
        "log in to it once, yourself (marion never runs this):\n  {}",
        login_command(harness, carrier.env, &dir, carrier.login_hint)
    )?;
    writeln!(
        out,
        "then name it on an agent type (`profile = \"{name}\"`), with `marion run --profile \
         {name}`, or make it {}'s default with `marion profile use {} {name}`",
        display_name(harness),
        display_name(harness)
    )?;
    Ok(())
}

/// Link each of the carrier's shared names from the harness's default directory into `dir`, where
/// the source exists and the target does not. **Never** a credential: the carrier's list is
/// non-secret by the row sweep, and nothing is copied — a link, so the operator's settings stay
/// one file.
fn link_shared(
    home: Option<&Path>,
    carrier: &marion_harness::profile::ProfileCarrier,
    dir: &Path,
) -> Result<Vec<String>, Refusal> {
    let Some(home) = home else {
        return Ok(Vec::new());
    };
    let source_root = home.join(carrier.home_default);
    let mut linked = Vec::new();
    for name in carrier.shared {
        let (source, target) = (source_root.join(name), dir.join(name));
        if source.exists() && target.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(&source, &target)?;
            linked.push((*name).to_string());
        }
    }
    Ok(linked)
}

/// `CLAUDE_CONFIG_DIR='<dir>' claude auth login` — one shell word per value.
pub fn login_command(harness: Harness, env: &str, dir: &str, hint: &str) -> String {
    let program = display_name(harness);
    let quoted = format!("'{}'", dir.replace('\'', r"'\''"));
    match hint {
        "" => format!("{env}={quoted} {program}   (sign in on its first screen)"),
        hint => format!("{env}={quoted} {program} {hint}"),
    }
}

/// **`list`**: every profile, its login as the harness's own probe reports it, when it was last
/// used, and the last usage reading a child's stream left — each with its age.
fn list(paths: &ProfilePaths, out: &mut dyn Write) -> Result<(), Refusal> {
    let rows = listed(paths)?;
    if rows.is_empty() {
        writeln!(
            out,
            "no profiles in {}; add one with `marion profile add <harness> <name>`",
            paths.config.display()
        )?;
        return Ok(());
    }
    let now = crate::clock::unix_millis();
    for Listed {
        profile,
        default,
        usage,
    } in rows
    {
        let login = match login_state(&profile) {
            LoginState::LoggedIn => "logged in".to_string(),
            LoginState::LoggedOut => "logged out".to_string(),
            LoginState::Unknown(why) => format!("login unknown ({why})"),
        };
        let used = usage
            .last_used_ms
            .map(|t| format!("used {}", age(now, t)))
            .unwrap_or_else(|| "never used".into());
        writeln!(
            out,
            "{}{} ({}) — {login}, {used}",
            profile.name,
            if default { " [default]" } else { "" },
            display_name(profile.harness)
        )?;
        writeln!(out, "  {}", profile.dir)?;
        if let Some(limit) = usage.limit {
            let window = limit.window.as_deref().unwrap_or("usage");
            let resets =
                marion_harness::resets_phrase(limit.line.as_deref().unwrap_or(""), limit.resets_at)
                    .map(|r| format!(", {r}"))
                    .unwrap_or_default();
            writeln!(
                out,
                "  last limit reading: {} ({window}){resets} — seen {}",
                limit.status,
                age(now, limit.observed_ms)
            )?;
        }
    }
    Ok(())
}

/// How long ago `then_ms` was, in the largest whole unit.
fn age(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// **`use`**: make `name` the default profile of `harness`.
fn use_default(
    paths: &ProfilePaths,
    harness: &str,
    name: &str,
    out: &mut dyn Write,
) -> Result<(), Refusal> {
    let harness = harness_named(harness)?;
    carrier(harness)?;
    let mut file = ProfilesFile::load(&paths.config)?;
    let entry = file.find(name).ok_or_else(|| ProfileError::Unknown {
        name: name.into(),
        harness: harness.as_str().into(),
    })?;
    if entry.harness != harness.as_str() {
        return Err(ProfileError::HarnessMismatch {
            name: name.into(),
            profile_harness: entry.harness.clone(),
            harness: harness.as_str().into(),
        }
        .into());
    }
    file.default
        .insert(harness.as_str().to_string(), name.to_string());
    file.save(&paths.config)?;
    writeln!(
        out,
        "{} nodes now run on profile `{name}` unless an agent type or a run names another",
        display_name(harness)
    )?;
    Ok(())
}

/// **`remove`**: forget the profile; with `--purge`, delete the directory marion created for it —
/// never a directory the operator adopted with `--dir`, which is theirs.
fn remove(
    paths: &ProfilePaths,
    name: &str,
    purge: bool,
    out: &mut dyn Write,
) -> Result<(), Refusal> {
    let mut file = ProfilesFile::load(&paths.config)?;
    let Some(at) = file.profile.iter().position(|p| p.name == name) else {
        return Err(ProfileError::Unknown {
            name: name.into(),
            harness: "<harness>".into(),
        }
        .into());
    };
    let entry = file.profile.remove(at);
    file.default.retain(|_, n| n != name);
    file.save(&paths.config)?;
    writeln!(
        out,
        "removed profile `{name}` from {}",
        paths.config.display()
    )?;
    let dir = Path::new(&entry.dir);
    if !purge {
        writeln!(out, "its directory is kept: {}", entry.dir)?;
    } else if dir.starts_with(&paths.data) {
        std::fs::remove_dir_all(dir).or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })?;
        writeln!(
            out,
            "deleted {} (a login the harness keeps outside it, such as a keychain entry, is the \
             harness's to remove)",
            entry.dir
        )?;
    } else {
        writeln!(
            out,
            "not deleted: {} is a directory you adopted with --dir, not one marion made",
            entry.dir
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bed(tag: &str) -> (marion_testsupport::Scratch, ProfilePaths, PathBuf) {
        let s = marion_testsupport::scratch(tag);
        let paths = ProfilePaths {
            config: s.join("config/marion/profiles.toml"),
            data: s.join("data/marion/profiles"),
            state: s.join("state/profiles"),
        };
        let home = s.join("home");
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "secret").unwrap();
        (s, paths, home)
    }

    fn verb(paths: &ProfilePaths, home: &Path, words: &[&str]) -> Result<String, Refusal> {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        let mut out = Vec::new();
        run(&argv, paths, Some(home), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn add_makes_the_directory_links_only_shared_files_and_prints_the_login() {
        let (_s, paths, home) = bed("profile-cli-add");
        let out = verb(&paths, &home, &["add", "claude", "work"]).unwrap();
        let dir = paths.dir_for(Harness::ClaudeCode, "work");
        assert!(dir.is_dir());
        assert!(
            dir.join("settings.json")
                .symlink_metadata()
                .unwrap()
                .is_symlink()
        );
        assert!(dir.join("skills").symlink_metadata().unwrap().is_symlink());
        assert!(
            dir.join(".credentials.json").symlink_metadata().is_err(),
            "a credential is never linked"
        );
        assert!(
            out.contains(&format!(
                "CLAUDE_CONFIG_DIR='{}' claude auth login",
                dir.display()
            )),
            "{out}"
        );
        let file = ProfilesFile::load(&paths.config).unwrap();
        assert_eq!(file.find("work").unwrap().dir, dir.to_string_lossy());
        assert!(
            file.default.is_empty(),
            "adding does not change the default login"
        );
    }

    #[test]
    fn add_adopts_an_existing_directory_exactly_as_typed() {
        let (s, paths, home) = bed("profile-cli-adopt");
        let own = s.join("own .claude-work");
        verb(
            &paths,
            &home,
            &["add", "codex", "cx", "--dir", own.to_str().unwrap()],
        )
        .unwrap();
        let file = ProfilesFile::load(&paths.config).unwrap();
        assert_eq!(file.find("cx").unwrap().dir, own.to_string_lossy());
        assert!(own.is_dir());
    }

    #[test]
    fn add_refuses_a_duplicate_a_bad_name_and_a_harness_without_a_carrier() {
        let (_s, paths, home) = bed("profile-cli-refuse");
        verb(&paths, &home, &["add", "claude", "work"]).unwrap();
        for words in [
            &["add", "codex", "work"][..],
            &["add", "claude", "../x"],
            &["add", "copilot", "gh"],
            &["add", "vim", "v"],
            &["add", "claude"],
        ] {
            assert!(verb(&paths, &home, words).is_err(), "{words:?}");
        }
    }

    #[test]
    fn use_sets_the_default_and_remove_forgets_and_purges_only_marions_directory() {
        let (s, paths, home) = bed("profile-cli-use");
        verb(&paths, &home, &["add", "claude", "work"]).unwrap();
        let own = s.join("adopted");
        verb(
            &paths,
            &home,
            &["add", "claude", "mine", "--dir", own.to_str().unwrap()],
        )
        .unwrap();
        assert!(
            verb(&paths, &home, &["use", "codex", "work"]).is_err(),
            "harness mismatch"
        );
        verb(&paths, &home, &["use", "claude", "work"]).unwrap();
        assert_eq!(
            ProfilesFile::load(&paths.config).unwrap().default["claude-code"],
            "work"
        );
        verb(&paths, &home, &["remove", "work", "--purge"]).unwrap();
        let file = ProfilesFile::load(&paths.config).unwrap();
        assert!(file.find("work").is_none() && file.default.is_empty());
        assert!(!paths.dir_for(Harness::ClaudeCode, "work").exists());
        let out = verb(&paths, &home, &["remove", "mine", "--purge"]).unwrap();
        assert!(
            own.is_dir(),
            "an adopted directory is the operator's: {out}"
        );
    }

    #[test]
    fn list_with_no_profiles_says_how_to_add_one() {
        let (_s, paths, home) = bed("profile-cli-empty");
        let out = verb(&paths, &home, &["list"]).unwrap();
        assert!(out.contains("marion profile add"), "{out}");
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        assert_eq!(age(10_000, 5_000), "5s ago");
        assert_eq!(age(3_600_000, 0), "1h ago");
        assert_eq!(age(0, 5_000), "0s ago");
    }
}
