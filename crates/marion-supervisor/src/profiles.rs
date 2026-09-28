//! **Profiles, supervisor side**: which account a node runs on, when the operator has more than
//! one login for a harness (work, personal, an org).
//!
//! A profile is a name, a harness, and a directory the operator logged into with that harness's
//! own login. They live in one file, `$XDG_CONFIG_HOME/marion/profiles.toml`:
//!
//! ```toml
//! [default]
//! claude-code = "work"
//!
//! [[profile]]
//! name = "work"
//! harness = "claude-code"
//! dir = "/Users/me/.local/share/marion/profiles/claude-code/work"
//! ```
//!
//! `dir` is stored **as the exact string the harness is handed**: claude keys its keychain entry
//! on a hash of the exported `CLAUDE_CONFIG_DIR`, so a path marion normalised would name a
//! different, logged-out account.
//!
//! # The rules this module keeps
//!
//! * marion never logs in, never copies or links a credential, and never reads one. A profile's
//!   status is the harness's own read-only probe ([`login_state`]), or a file's existence.
//! * A **usage limit is a notice, never a reason to change account** ([`limit_notice`] says what
//!   happened and when it resets, and nothing else). Only an expired or refused login fails over,
//!   and only to a profile the agent type listed after the one that failed ([`failover_target`]).
//! * A resume runs on the profile the node's session was recorded under, never on a re-resolved
//!   one.
//!
//! What differs per harness — the variable, the probe, the login command — is the row's
//! [`ProfileCarrier`]; nothing here names a harness.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use marion_core::contract::FailureCause;
use marion_core::harness::Harness;
use marion_harness::adapter::harness_spec;
use marion_harness::grammar::LimitReading;
use marion_harness::profile::{ProfileCarrier, Status};
use serde::{Deserialize, Serialize};

/// The file's name under `$XDG_CONFIG_HOME/marion/`.
pub const CONFIG_FILE: &str = "profiles.toml";

/// How long a status probe may take before its answer is "unknown".
const STATUS_BOUND: Duration = Duration::from_secs(20);

/// Where profiles live: the one config file, the directories marion creates for new profiles, and
/// the usage readings children's streams left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePaths {
    /// `$XDG_CONFIG_HOME/marion/profiles.toml`, else `~/.config/marion/profiles.toml`.
    pub config: PathBuf,
    /// `$XDG_DATA_HOME/marion/profiles`, else `~/.local/share/marion/profiles`.
    pub data: PathBuf,
    /// `<state>/profiles` — the readings `marion profile list` shows. Never a credential.
    pub state: PathBuf,
}

impl ProfilePaths {
    /// Resolved from this process's environment, or `None` when even `$HOME` is absent.
    pub fn from_env() -> Option<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// Resolved from `var`. An empty value counts as unset, [`marion_core::paths::state_dir`]'s
    /// rule, so `XDG_CONFIG_HOME=` never roots a profile at the cwd.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let set = |k: &str| var(k).filter(|v| !v.is_empty());
        let home = set("HOME");
        let under_home = |rel: &str| home.as_ref().map(|h| Path::new(h).join(rel));
        let config = set("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| under_home(".config"))?;
        let data = set("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| under_home(".local/share"))?;
        let state = marion_core::paths::state_dir(
            set("MARION_STATE_DIR").as_deref(),
            set("XDG_STATE_HOME").as_deref(),
            home.as_deref(),
        )?;
        Some(Self {
            config: config.join("marion").join(CONFIG_FILE),
            data: data.join("marion").join("profiles"),
            state: state.join("profiles"),
        })
    }

    /// The same paths with the usage readings under another state root — a supervisor started
    /// with `--state-dir` keeps its readings beside its journal.
    pub fn with_state_root(mut self, state_root: &Path) -> Self {
        self.state = state_root.join("profiles");
        self
    }

    /// Where `marion profile add` creates a profile's directory.
    pub fn dir_for(&self, harness: Harness, name: &str) -> PathBuf {
        self.data.join(harness.as_str()).join(name)
    }

    fn usage_file(&self, harness: Harness, name: &str) -> PathBuf {
        self.state
            .join(harness.as_str())
            .join(format!("{name}.json"))
    }
}

/// Why a profile could not be used. Each refusal names the command that fixes it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("{path}: {why}")]
    Config { path: String, why: String },
    #[error(
        "no profile named `{name}`; create it with `marion profile add {harness} {name}` and log \
         in with the command it prints"
    )]
    Unknown { name: String, harness: String },
    #[error("profile `{name}` is a {profile_harness} profile, and this node runs {harness}")]
    HarnessMismatch {
        name: String,
        profile_harness: String,
        harness: String,
    },
    #[error(
        "profile `{name}`'s directory {dir} does not exist; re-create it with `marion profile add \
         {harness} {name}`"
    )]
    MissingDir {
        name: String,
        harness: String,
        dir: String,
    },
    #[error("{harness} has no profile carrier: marion cannot point it at a per-account directory")]
    NoCarrier { harness: String },
    #[error(
        "profile name {0:?} is not a name: profile names match ^[A-Za-z0-9][A-Za-z0-9_-]{{0,63}}$"
    )]
    InvalidName(String),
    #[error("a profile named `{0}` already exists")]
    Duplicate(String),
    #[error("{0}")]
    Io(String),
}

/// The file as written. Validated by [`ProfilesFile::parse`], so every value a reader sees is one
/// a launch can use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesFile {
    /// Harness wire name → the profile a node of that harness runs on when nothing else says.
    #[serde(default)]
    pub default: BTreeMap<String, String>,
    #[serde(default)]
    pub profile: Vec<ProfileEntry>,
}

/// One `[[profile]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEntry {
    pub name: String,
    pub harness: String,
    pub dir: String,
}

/// A profile a launch can use: its harness parsed, its directory as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub harness: Harness,
    pub dir: String,
}

impl ProfilesFile {
    /// The file at `path`, or the empty table where there is none. Any other failure is a
    /// refusal naming the path: a mistyped file must not silently run every node on the default
    /// login.
    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| match e {
                ProfileError::Config { why, .. } => ProfileError::Config {
                    path: path.display().to_string(),
                    why,
                },
                other => other,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ProfileError::Config {
                path: path.display().to_string(),
                why: e.to_string(),
            }),
        }
    }

    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        let config = |why: String| ProfileError::Config {
            path: CONFIG_FILE.into(),
            why,
        };
        let file: Self = toml::from_str(text).map_err(|e| config(e.to_string()))?;
        for (i, p) in file.profile.iter().enumerate() {
            if !marion_core::agent_type::is_valid_name(&p.name) {
                return Err(ProfileError::InvalidName(p.name.clone()));
            }
            if file.profile[..i].iter().any(|q| q.name == p.name) {
                return Err(ProfileError::Duplicate(p.name.clone()));
            }
            p.harness
                .parse::<Harness>()
                .map_err(|e| config(format!("profile `{}`: {e}", p.name)))?;
        }
        for (harness, name) in &file.default {
            let ok = file
                .profile
                .iter()
                .any(|p| &p.name == name && &p.harness == harness);
            if !ok {
                return Err(config(format!(
                    "[default] {harness} = {name:?} names no {harness} profile"
                )));
            }
        }
        Ok(file)
    }

    /// The file's text. Hand-written rather than serialised so the layout is the one documented
    /// above, and so a round trip through [`Self::parse`] is exact.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# marion profiles: one directory per account, logged into with the harness's own \
             login.\n# Edit with `marion profile add|use|remove`.\n",
        );
        if !self.default.is_empty() {
            out.push_str("\n[default]\n");
            for (harness, name) in &self.default {
                out.push_str(&format!("{} = {}\n", toml_key(harness), toml_str(name)));
            }
        }
        for p in &self.profile {
            out.push_str(&format!(
                "\n[[profile]]\nname = {}\nharness = {}\ndir = {}\n",
                toml_str(&p.name),
                toml_str(&p.harness),
                toml_str(&p.dir)
            ));
        }
        out
    }

    /// Write the file, replacing it in one rename so a reader never sees half of it.
    pub fn save(&self, path: &Path) -> Result<(), ProfileError> {
        write_atomically(path, self.render().as_bytes())
    }

    pub fn find(&self, name: &str) -> Option<&ProfileEntry> {
        self.profile.iter().find(|p| p.name == name)
    }
}

/// A bare key where TOML allows one (`claude-code`), else a quoted one.
fn toml_key(k: &str) -> String {
    if !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        k.to_string()
    } else {
        toml_str(k)
    }
}

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), ProfileError> {
    let io = |e: std::io::Error| ProfileError::Io(format!("{}: {e}", path.display()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// A harness named the way an operator types it: its wire name (`claude-code`) or its row's
/// program (`claude`).
pub fn parse_harness(s: &str) -> Option<Harness> {
    s.parse::<Harness>().ok().or_else(|| {
        Harness::ALL
            .into_iter()
            .find(|h| harness_spec(*h).program == Some(s))
    })
}

/// How a notice names a harness: its program where the row has one (`claude`).
pub fn display_name(harness: Harness) -> &'static str {
    harness_spec(harness).program.unwrap_or(harness.as_str())
}

/// The row's carrier, or the refusal a profile on a carrier-less harness gets.
pub fn carrier(harness: Harness) -> Result<&'static ProfileCarrier, ProfileError> {
    harness_spec(harness)
        .profile
        .as_ref()
        .ok_or_else(|| ProfileError::NoCarrier {
            harness: harness.as_str().into(),
        })
}

/// **The profiles a node of `harness` runs on, first to last**: the first is the account the node
/// uses, and the rest are its failover order for an expired or refused login.
///
/// Resolution is **spawn > agent type > `[default]` > none**. A spawn that names a profile pins
/// that one alone — an explicit choice for one launch has no failover. An empty answer is today's
/// behaviour: the harness's own default login, untouched.
///
/// Every named profile is checked here, before a node exists: an unknown name, a profile of
/// another harness, a directory that is gone, and a harness whose row has no carrier are each
/// refused by name.
pub fn resolve(
    file: &ProfilesFile,
    harness: Harness,
    requested: Option<&str>,
    agent_type: &[String],
) -> Result<Vec<Profile>, ProfileError> {
    let names: Vec<&str> = match requested {
        Some(name) => vec![name],
        None if !agent_type.is_empty() => agent_type.iter().map(String::as_str).collect(),
        None => file
            .default
            .get(harness.as_str())
            .map(|n| vec![n.as_str()])
            .unwrap_or_default(),
    };
    if names.is_empty() {
        return Ok(Vec::new());
    }
    carrier(harness)?;
    names
        .into_iter()
        .map(|n| usable(file, harness, n))
        .collect()
}

/// One named profile, checked for this harness.
fn usable(file: &ProfilesFile, harness: Harness, name: &str) -> Result<Profile, ProfileError> {
    let entry = file.find(name).ok_or_else(|| ProfileError::Unknown {
        name: name.into(),
        harness: harness.as_str().into(),
    })?;
    if entry.harness != harness.as_str() {
        return Err(ProfileError::HarnessMismatch {
            name: name.into(),
            profile_harness: entry.harness.clone(),
            harness: harness.as_str().into(),
        });
    }
    if !Path::new(&entry.dir).is_dir() {
        return Err(ProfileError::MissingDir {
            name: name.into(),
            harness: harness.as_str().into(),
            dir: entry.dir.clone(),
        });
    }
    Ok(Profile {
        name: entry.name.clone(),
        harness,
        dir: entry.dir.clone(),
    })
}

/// A profile recorded on a node, looked up again for its resume. The name is the node's; the
/// directory is today's entry for it, and a profile that has since been removed is refused rather
/// than silently replaced by another account.
pub fn recorded(
    file: &ProfilesFile,
    harness: Harness,
    name: &str,
) -> Result<Profile, ProfileError> {
    carrier(harness)?;
    usable(file, harness, name)
}

/// **The only failover there is**: after a run on `chain[current]` whose cause is an expired or
/// refused login, the next listed profile — never after a usage limit, an outage, or a failure
/// marion could not classify.
pub fn failover_target(
    chain: &[Profile],
    current: usize,
    cause: Option<&FailureCause>,
) -> Option<usize> {
    let next = current + 1;
    (matches!(cause, Some(FailureCause::Auth { .. })) && next < chain.len()).then_some(next)
}

/// What the harness's own probe says about a profile's login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    LoggedIn,
    LoggedOut,
    /// The probe could not answer; the reason, for the operator.
    Unknown(String),
}

/// **Ask the harness, never a credential**: the row's read-only status probe, run with the
/// profile's variable set, or the existence of the file the row names — whose contents are never
/// opened.
pub fn login_state(profile: &Profile) -> LoginState {
    let carrier = match carrier(profile.harness) {
        Ok(c) => c,
        Err(e) => return LoginState::Unknown(e.to_string()),
    };
    let argv = match carrier.status {
        Status::FileExists(rel) => {
            return match Path::new(&profile.dir).join(rel).symlink_metadata() {
                Ok(_) => LoginState::LoggedIn,
                Err(_) => LoginState::LoggedOut,
            };
        }
        Status::JsonBool { argv, .. } | Status::TextAbsent { argv, .. } => argv,
    };
    let Some(program) = harness_spec(profile.harness).program else {
        return LoginState::Unknown("the row names no program to ask".into());
    };
    let (switch_env, switch_args) = match update_switch(profile.harness) {
        Ok(switch) => switch,
        Err(why) => return LoginState::Unknown(why),
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(&switch_args)
        .args(argv)
        .envs(switch_env)
        .env(carrier.env, &profile.dir)
        .stdin(std::process::Stdio::null());
    for key in carrier.clear {
        cmd.env_remove(key);
    }
    let out = match crate::run::run_bounded(&mut cmd, STATUS_BOUND) {
        Ok(out) if out.timed_out => {
            return LoginState::Unknown(format!(
                "`{program} {}` did not answer within {}s",
                argv.join(" "),
                STATUS_BOUND.as_secs()
            ));
        }
        Ok(out) => out,
        Err(e) => return LoginState::Unknown(format!("`{program}` could not run: {e}")),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    match carrier.status {
        Status::JsonBool { key, .. } => json_bool(&stdout, key),
        Status::TextAbsent { text, .. } => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            text_absent(&format!("{stdout}{stderr}"), out.code, text)
        }
        Status::FileExists(_) => LoginState::Unknown("answered above".into()),
    }
}

/// A probe's share of the row's update switch: the variables it sets, and the arguments ahead of
/// its own.
type ProbeSwitch = (Vec<(String, String)>, Vec<String>);

/// **The row's no-self-update switch, as a bare probe of the installed binary carries it**: the
/// variable, or the pair on the row's own override flag ahead of the probe's argv. A probe is a
/// launch of the operator's real binary, and a launch without the switch can update their install.
/// `Err` — and no probe — where the switch cannot ride a bare command (a settings document, or a
/// pair the row has no flag for).
fn update_switch(harness: Harness) -> Result<ProbeSwitch, String> {
    use marion_harness::spec::{Arg, Field, UpdatePolicy};
    let spec = harness_spec(harness);
    match spec.updates {
        UpdatePolicy::Env { key, value, .. } => Ok((vec![(key.into(), value.into())], Vec::new())),
        UpdatePolicy::Pair { key, value, .. } => spec
            .argv
            .iter()
            .find_map(|arg| match *arg {
                Arg::Each(flag, Field::Pairs) => Some(vec![flag.into(), format!("{key}={value}")]),
                Arg::EachEq(flag, Field::Pairs) => Some(vec![format!("{flag}={key}={value}")]),
                _ => None,
            })
            .map(|args| (Vec::new(), args))
            .ok_or_else(|| format!("{key}={value} has no override flag to ride; not probed")),
        UpdatePolicy::Document { .. } => Err(
            "the row's update switch is a settings document a bare probe cannot carry; not probed"
                .into(),
        ),
        UpdatePolicy::None { .. } => Ok((Vec::new(), Vec::new())),
    }
}

/// The probe's JSON object on stdout — pretty-printed on claude, so read whole.
fn json_bool(stdout: &str, key: &str) -> LoginState {
    let value = serde_json::from_str::<serde_json::Value>(stdout.trim())
        .ok()
        .and_then(|v| v.get(key).and_then(serde_json::Value::as_bool));
    match value {
        Some(true) => LoginState::LoggedIn,
        Some(false) => LoginState::LoggedOut,
        None => LoginState::Unknown(format!("the status probe carried no `{key}`")),
    }
}

fn text_absent(out: &str, code: Option<i32>, text: &str) -> LoginState {
    if out.contains(text) {
        LoginState::LoggedOut
    } else if code == Some(0) {
        LoginState::LoggedIn
    } else {
        LoginState::Unknown(format!("the status probe exited {code:?}"))
    }
}

/// What children's streams last said about a profile, and when it was last used. Read by
/// `marion profile list`; never a credential, never fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitSeen>,
}

/// One usage-window reading, as a stream stated it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitSeen {
    pub observed_ms: u64,
    /// The harness's word: `allowed`, `allowed_warning`, `rejected`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

impl LimitSeen {
    /// A stream's reading, stamped now.
    pub fn from_reading(r: &LimitReading, now_ms: u64) -> Self {
        Self {
            observed_ms: now_ms,
            status: r.status.clone(),
            window: r.window.clone(),
            resets_at: r.resets_at,
            line: None,
        }
    }

    /// A run that ended on a usage limit, stamped now: the harness's sentence stands in for a
    /// reading where the stream carried none.
    pub fn from_limit(line: &str, resets_at: Option<u64>, now_ms: u64) -> Self {
        Self {
            observed_ms: now_ms,
            status: "rejected".into(),
            window: Some(marion_harness::limit_window(line).into()),
            resets_at,
            line: Some(line.into()),
        }
    }
}

pub fn read_usage(paths: &ProfilePaths, harness: Harness, name: &str) -> Usage {
    std::fs::read(paths.usage_file(harness, name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn update_usage(paths: &ProfilePaths, profile: &Profile, change: impl FnOnce(&mut Usage)) {
    let mut usage = read_usage(paths, profile.harness, &profile.name);
    change(&mut usage);
    if let Ok(bytes) = serde_json::to_vec(&usage) {
        // Best effort: a reading that could not be written costs `profile list` one line, never a
        // run.
        let _ = write_atomically(&paths.usage_file(profile.harness, &profile.name), &bytes);
    }
}

/// A node was launched on `profile`.
pub fn record_used(paths: &ProfilePaths, profile: &Profile, now_ms: u64) {
    update_usage(paths, profile, |u| u.last_used_ms = Some(now_ms));
}

/// A stream stated `profile`'s usage window.
pub fn record_limit(paths: &ProfilePaths, profile: &Profile, seen: LimitSeen) {
    update_usage(paths, profile, |u| u.limit = Some(seen));
}

/// **The notice a usage limit gets, and all it gets**: which profile, which window, and when it
/// resets. No relaunch, no failover, and no suggestion to use another account — spending one
/// account's limit and moving to the next is not something marion does.
pub fn limit_notice(
    profile: Option<&Profile>,
    harness: Harness,
    line: &str,
    resets_at: Option<u64>,
) -> String {
    let who = match profile {
        Some(p) => format!("profile `{}` ({})", p.name, display_name(harness)),
        None => display_name(harness).to_string(),
    };
    let window = marion_harness::limit_window(line);
    let mut notice = format!("{who} hit its {window} limit");
    if let Some(resets) = marion_harness::resets_phrase(line, resets_at) {
        notice.push_str("; ");
        notice.push_str(&resets);
    }
    notice
}

/// **One node's profiles, for the length of its launch** — the chain [`resolve`] answered, where
/// to keep readings, and the three things a launch does with them: point each attempt at its
/// profile's directory, decide whether a failed attempt may fail over, and write the run's cause
/// onto its contract.
///
/// Empty — no profile, nothing recorded, every method a no-op — under canned auth (the canned rows
/// keep their own isolation) and wherever nothing selects a profile.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub chain: Vec<Profile>,
    pub paths: Option<ProfilePaths>,
}

impl Launch {
    /// The chain for a launch of `harness` — a child's or a root's: a resume runs on exactly the
    /// profile its session was recorded under (or none); a fresh launch resolves the run's own
    /// choice > agent type > default.
    pub fn resolve(
        auth: marion_harness::Auth,
        state_root: &Path,
        harness: Harness,
        requested: Option<&str>,
        resuming: bool,
        agent_type: &[String],
    ) -> Result<Self, ProfileError> {
        if auth != marion_harness::Auth::Inherited {
            return Ok(Self::default());
        }
        let paths = ProfilePaths::from_env().map(|p| p.with_state_root(state_root));
        let file = match &paths {
            Some(p) => ProfilesFile::load(&p.config)?,
            None => ProfilesFile::default(),
        };
        let chain = if resuming {
            match requested {
                Some(name) => vec![recorded(&file, harness, name)?],
                None => Vec::new(),
            }
        } else {
            resolve(&file, harness, requested, agent_type)?
        };
        Ok(Self { chain, paths })
    }

    pub fn profile(&self, at: usize) -> Option<&Profile> {
        self.chain.get(at)
    }

    /// The directory attempt `at` runs in, as stored.
    pub fn dir(&self, at: usize) -> Option<PathBuf> {
        self.profile(at).map(|p| PathBuf::from(&p.dir))
    }

    /// Attempt `at` is launching: stamp the profile's last use.
    pub fn used(&self, at: usize) {
        if let (Some(paths), Some(p)) = (&self.paths, self.profile(at)) {
            record_used(paths, p, crate::clock::unix_millis());
        }
    }

    /// A stream frame of attempt `at`: keep its usage-window reading, where the row reads one.
    pub fn observe(&self, at: usize, frame: &serde_json::Value) {
        let (Some(paths), Some(p)) = (&self.paths, self.profile(at)) else {
            return;
        };
        let rule = harness_spec(p.harness)
            .stream
            .and_then(|g| g.rate_limit.as_ref());
        if let Some(reading) = rule.and_then(|r| marion_harness::grammar::rate_limit(r, frame)) {
            record_limit(
                paths,
                p,
                LimitSeen::from_reading(&reading, crate::clock::unix_millis()),
            );
        }
    }

    /// **The profile after attempt `at`**, if the agent type listed one. The failover decision is
    /// the run's (`run::next_attempt`: an auth failure only, never a usage limit); this is only
    /// where it would go.
    pub fn next_after(&self, at: usize) -> Option<usize> {
        let next = at + 1;
        (next < self.chain.len()).then_some(next)
    }

    /// **Journal a failover from attempt `at` to `next`**, before the relaunch runs.
    pub fn record_failover(
        &self,
        at: usize,
        next: usize,
        cause: &FailureCause,
        project: &marion_core::paths::ProjectDir,
        agent_id: &marion_core::contract::AgentId,
    ) {
        debug_assert_eq!(failover_target(&self.chain, at, Some(cause)), Some(next));
        crate::journal::record(
            project,
            marion_core::journal::RecordKind::ProfileFailover(
                marion_core::journal::ProfileFailover {
                    agent_id: agent_id.clone(),
                    harness: self.chain[at].harness,
                    from: self.chain[at].name.clone(),
                    to: self.chain[next].name.clone(),
                    cause: "auth".into(),
                },
            ),
        );
    }

    /// **The run's cause, onto its contract** (the one classifier's, read with the node's own
    /// billing): [`Completion::failure_cause`] always, and for a usage limit the notice leads the
    /// exit description — the sentence a parent's `wait` and the
    /// announcement of a backgrounded child's end both quote — and the reading is kept for
    /// `marion profile list`. Nothing else happens on a limit.
    ///
    /// [`Completion::failure_cause`]: marion_core::contract::Completion::failure_cause
    pub fn settle(
        &self,
        at: usize,
        harness: Harness,
        contract: &mut marion_core::contract::TaskContract,
        cause: Option<FailureCause>,
    ) {
        let Some(cause) = cause else {
            return;
        };
        let Some(completion) = contract.completion.as_mut() else {
            return;
        };
        if let FailureCause::UsageLimit { line, resets_at } = &cause {
            let notice = limit_notice(self.profile(at), harness, line, *resets_at);
            completion.exit.description = format!("{notice}; {}", completion.exit.description);
            if let (Some(paths), Some(p)) = (&self.paths, self.profile(at)) {
                record_limit(
                    paths,
                    p,
                    LimitSeen::from_limit(line, *resets_at, crate::clock::unix_millis()),
                );
            }
        }
        completion.failure_cause = Some(cause);
    }
}

/// **An account is the operator's choice, never a node's**: `profile` on a spawn with a `caller`
/// is refused. A node's children run on their agent type's profile; a node that could pick one
/// could move its work to another account when its own hit a limit, which marion does not do.
pub fn check_child_profile(
    p: &marion_core::proto::params::AgentSpawnParams,
) -> Result<(), marion_core::proto::RpcError> {
    if p.caller.is_some() && p.profile.is_some() {
        return Err(marion_core::proto::RpcError::refused(
            "profile",
            "a spawn with a `caller` must not state `profile`: which login a node runs on is the \
             operator's choice, made on the agent type's `profile` or in profiles.toml, and a node \
             that could choose its children's account could move work to another account when its \
             own hit a usage limit. Refused rather than dropped (§11 item 23).",
            "profiles, §11 item 23",
        ));
    }
    Ok(())
}

/// The variable an operator sets to choose a native session's profile: `MARION_PROFILE=personal
/// marion claude`. Read from the operator's own environment, and never passed on to the harness —
/// every `MARION_` name is marion's.
pub const NATIVE_PROFILE_ENV: &str = "MARION_PROFILE";

/// **The profile a native `marion <harness>` session runs on, applied to its environment** — the
/// authoritative, already-assembled one the harness is started with. Chosen by the operator's
/// `MARION_PROFILE`, else the lane's agent type, else `profiles.toml`'s `[default]`, read from the
/// operator's own environment (`client_env`), so the session selects the profile the operator's
/// shell would. Without one, `env` is untouched.
pub fn apply_native(
    harness: Harness,
    agent_type: &[String],
    client_env: &[(std::ffi::OsString, std::ffi::OsString)],
    env: &mut Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<Option<Profile>, ProfileError> {
    let var = |k: &str| {
        client_env
            .iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, v)| v.to_str().map(str::to_string))
    };
    let Some(paths) = ProfilePaths::from_vars(var) else {
        return Ok(None);
    };
    let file = ProfilesFile::load(&paths.config)?;
    let requested = var(NATIVE_PROFILE_ENV).filter(|v| !v.is_empty());
    let Some(profile) = resolve(&file, harness, requested.as_deref(), agent_type)?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let carrier = carrier(harness)?;
    env.retain(|(k, _)| k != carrier.env && !carrier.clear.iter().any(|c| k == c));
    env.push((carrier.env.into(), profile.dir.clone().into()));
    record_used(&paths, &profile, crate::clock::unix_millis());
    Ok(Some(profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_native_session_takes_the_operators_profile_and_is_otherwise_untouched() {
        use std::ffi::OsString;
        let dir = marion_testsupport::scratch("profiles-native");
        let file = file_with(&dir);
        let config = dir.join("config");
        std::fs::create_dir_all(config.join("marion")).unwrap();
        file.save(&config.join("marion/profiles.toml")).unwrap();
        let pair = |k: &str, v: &str| (OsString::from(k), OsString::from(v));
        let client = |extra: Option<(OsString, OsString)>| {
            let mut e = vec![
                pair("HOME", &dir.to_string_lossy()),
                pair("XDG_CONFIG_HOME", &config.to_string_lossy()),
            ];
            e.extend(extra);
            e
        };
        let base = vec![
            pair("PATH", "/bin"),
            pair("CLAUDE_SECURESTORAGE_CONFIG_DIR", "/elsewhere"),
            pair("CLAUDE_CONFIG_DIR", "/operators/own"),
        ];

        let mut env = base.clone();
        let chosen = apply_native(
            Harness::ClaudeCode,
            &[],
            &client(Some(pair("MARION_PROFILE", "personal"))),
            &mut env,
        )
        .unwrap();
        assert_eq!(chosen.map(|p| p.name).as_deref(), Some("personal"));
        let personal = file.find("personal").unwrap().dir.clone();
        assert_eq!(
            env,
            vec![pair("PATH", "/bin"), pair("CLAUDE_CONFIG_DIR", &personal)]
        );

        let mut env = base.clone();
        apply_native(Harness::ClaudeCode, &[], &client(None), &mut env).unwrap();
        assert!(env.contains(&pair("CLAUDE_CONFIG_DIR", &file.find("work").unwrap().dir)));

        let mut env = base.clone();
        let none = apply_native(Harness::Codex, &[], &client(None), &mut env).unwrap();
        assert!(none.is_none());
        assert_eq!(
            env, base,
            "no profile: the session's environment is untouched"
        );

        let mut env = base.clone();
        assert!(matches!(
            apply_native(
                Harness::ClaudeCode,
                &[],
                &client(Some(pair("MARION_PROFILE", "nope"))),
                &mut env
            ),
            Err(ProfileError::Unknown { .. })
        ));
    }

    fn entry(name: &str, harness: &str, dir: &str) -> ProfileEntry {
        ProfileEntry {
            name: name.into(),
            harness: harness.into(),
            dir: dir.into(),
        }
    }

    fn file_with(dir: &Path) -> ProfilesFile {
        let d = |n: &str| {
            let p = dir.join(n);
            std::fs::create_dir_all(&p).unwrap();
            p.to_string_lossy().into_owned()
        };
        ProfilesFile {
            default: BTreeMap::from([("claude-code".to_string(), "work".to_string())]),
            profile: vec![
                entry("work", "claude-code", &d("work")),
                entry("personal", "claude-code", &d("personal")),
                entry("cx", "codex", &d("cx")),
                entry("gone", "claude-code", "/nonexistent/marion-profile"),
            ],
        }
    }

    /// Every carrier whose status is a probe launches its binary with the row's update switch:
    /// claude's variable, codex's pair on its own `-c`. A carrier the switch cannot ride is not
    /// probed at all.
    #[test]
    fn a_status_probe_carries_its_rows_update_switch() {
        assert_eq!(
            update_switch(Harness::ClaudeCode),
            Ok((
                vec![("DISABLE_AUTOUPDATER".to_string(), "1".to_string())],
                Vec::new()
            ))
        );
        assert_eq!(
            update_switch(Harness::Codex),
            Ok((
                Vec::new(),
                vec![
                    "-c".to_string(),
                    "check_for_update_on_startup=false".to_string()
                ]
            ))
        );
        for harness in Harness::ALL {
            let Some(carrier) = harness_spec(harness).profile.as_ref() else {
                continue;
            };
            if matches!(carrier.status, Status::FileExists(_)) {
                continue;
            }
            let (vars, args) = update_switch(harness)
                .unwrap_or_else(|why| panic!("{harness:?}'s probe cannot carry its switch: {why}"));
            assert!(
                !(vars.is_empty() && args.is_empty())
                    || matches!(
                        harness_spec(harness).updates,
                        marion_harness::spec::UpdatePolicy::None { .. }
                    ),
                "{harness:?}'s probe launches without the row's switch"
            );
        }
    }

    #[test]
    fn paths_follow_xdg_and_fall_back_to_home() {
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let xdg = ProfilePaths::from_vars(vars(&[
            ("HOME", "/h"),
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_DATA_HOME", "/d"),
            ("XDG_STATE_HOME", "/s"),
        ]))
        .unwrap();
        assert_eq!(xdg.config, Path::new("/c/marion/profiles.toml"));
        assert_eq!(
            xdg.dir_for(Harness::ClaudeCode, "work"),
            Path::new("/d/marion/profiles/claude-code/work")
        );
        assert_eq!(xdg.state, Path::new("/s/marion/profiles"));
        let home =
            ProfilePaths::from_vars(vars(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "")])).unwrap();
        assert_eq!(home.config, Path::new("/h/.config/marion/profiles.toml"));
        assert_eq!(home.data, Path::new("/h/.local/share/marion/profiles"));
        assert_eq!(ProfilePaths::from_vars(vars(&[])), None);
    }

    #[test]
    fn the_file_round_trips_its_own_rendering_exactly() {
        let file = ProfilesFile {
            default: BTreeMap::from([("claude-code".to_string(), "work".to_string())]),
            profile: vec![
                entry("work", "claude-code", "/p/with \"quote\" and \\slash"),
                entry("cx", "codex", "/p/cx"),
            ],
        };
        let text = file.render();
        assert_eq!(ProfilesFile::parse(&text), Ok(file));
        assert!(text.contains("[default]\nclaude-code = \"work\""), "{text}");
    }

    #[test]
    fn a_malformed_file_is_refused_and_never_read_as_empty() {
        for bad in [
            "[[profile]]\nname = \"a\"\nharness = \"claude-code\"\n",
            "[[profile]]\nname = \"a/b\"\nharness = \"claude-code\"\ndir = \"/x\"\n",
            "[[profile]]\nname = \"a\"\nharness = \"vim\"\ndir = \"/x\"\n",
            "[default]\ncodex = \"nobody\"\n",
            "[[profile]]\nname = \"a\"\nharness = \"codex\"\ndir = \"/x\"\n\
             [[profile]]\nname = \"a\"\nharness = \"codex\"\ndir = \"/y\"\n",
            "stray = 1\n",
        ] {
            assert!(ProfilesFile::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(ProfilesFile::parse(""), Ok(ProfilesFile::default()));
        assert_eq!(
            ProfilesFile::load(Path::new("/nonexistent/profiles.toml")),
            Ok(ProfilesFile::default())
        );
    }

    #[test]
    fn resolution_is_spawn_then_agent_type_then_default_then_none() {
        let dir = marion_testsupport::scratch("profiles-resolve");
        let file = file_with(&dir);
        let names = |r: Result<Vec<Profile>, ProfileError>| {
            r.unwrap().into_iter().map(|p| p.name).collect::<Vec<_>>()
        };
        let listed = vec!["personal".to_string(), "work".to_string()];
        assert_eq!(
            names(resolve(&file, Harness::ClaudeCode, Some("personal"), &[])),
            ["personal"]
        );
        assert_eq!(
            names(resolve(&file, Harness::ClaudeCode, Some("work"), &listed)),
            ["work"],
            "a spawn's choice pins one profile, with no failover"
        );
        assert_eq!(
            names(resolve(&file, Harness::ClaudeCode, None, &listed)),
            ["personal", "work"]
        );
        assert_eq!(
            names(resolve(&file, Harness::ClaudeCode, None, &[])),
            ["work"]
        );
        assert!(names(resolve(&file, Harness::Codex, None, &[])).is_empty());
        assert!(
            names(resolve(
                &ProfilesFile::default(),
                Harness::Copilot,
                None,
                &[]
            ))
            .is_empty()
        );
    }

    #[test]
    fn every_unusable_profile_is_refused_by_name() {
        let dir = marion_testsupport::scratch("profiles-refuse");
        let file = file_with(&dir);
        let err = |r: Result<Vec<Profile>, ProfileError>| r.unwrap_err();
        let unknown = err(resolve(&file, Harness::ClaudeCode, Some("nope"), &[]));
        assert!(matches!(unknown, ProfileError::Unknown { .. }));
        assert!(
            unknown
                .to_string()
                .contains("marion profile add claude-code nope")
        );
        assert!(matches!(
            err(resolve(&file, Harness::ClaudeCode, Some("cx"), &[])),
            ProfileError::HarnessMismatch { .. }
        ));
        let missing = err(resolve(&file, Harness::ClaudeCode, Some("gone"), &[]));
        assert!(
            missing.to_string().contains("marion profile add"),
            "{missing}"
        );
        assert!(matches!(
            err(resolve(&file, Harness::Copilot, Some("work"), &[])),
            ProfileError::NoCarrier { .. }
        ));
    }

    #[test]
    fn only_an_auth_failure_fails_over_and_only_to_a_listed_profile() {
        let chain: Vec<Profile> = ["a", "b"]
            .iter()
            .map(|n| Profile {
                name: n.to_string(),
                harness: Harness::ClaudeCode,
                dir: format!("/p/{n}"),
            })
            .collect();
        let auth = FailureCause::Auth {
            line: "Invalid API key".into(),
        };
        assert_eq!(failover_target(&chain, 0, Some(&auth)), Some(1));
        assert_eq!(failover_target(&chain, 1, Some(&auth)), None, "the last");
        let limit = FailureCause::UsageLimit {
            line: "You've hit your session limit".into(),
            resets_at: None,
        };
        let outage = FailureCause::Outage { line: "529".into() };
        for cause in [Some(&limit), Some(&outage), None] {
            assert_eq!(failover_target(&chain, 0, cause), None, "{cause:?}");
        }
    }

    #[test]
    fn a_limit_notice_names_the_profile_the_window_and_the_reset_and_nothing_else() {
        let work = Profile {
            name: "work".into(),
            harness: Harness::ClaudeCode,
            dir: "/p".into(),
        };
        let notice = limit_notice(
            Some(&work),
            Harness::ClaudeCode,
            "You've hit your session limit · resets 7:50pm",
            None,
        );
        assert_eq!(
            notice,
            "profile `work` (claude) hit its session limit; resets 7:50pm"
        );
        for swap in ["switch", "another", "profile use", "try"] {
            assert!(!notice.contains(swap), "no swap suggestion: {notice}");
        }
        assert_eq!(
            limit_notice(None, Harness::Codex, "usage_limit_exceeded", None),
            "codex hit its usage limit"
        );
    }

    #[test]
    fn usage_readings_round_trip_and_a_missing_one_is_empty() {
        let dir = marion_testsupport::scratch("profiles-usage");
        let paths = ProfilePaths {
            config: dir.join("c/profiles.toml"),
            data: dir.join("d"),
            state: dir.join("s"),
        };
        let work = Profile {
            name: "work".into(),
            harness: Harness::ClaudeCode,
            dir: "/p".into(),
        };
        assert_eq!(read_usage(&paths, work.harness, "work"), Usage::default());
        record_used(&paths, &work, 5);
        record_limit(
            &paths,
            &work,
            LimitSeen::from_limit("hit your weekly limit", Some(9), 7),
        );
        let usage = read_usage(&paths, work.harness, "work");
        assert_eq!(usage.last_used_ms, Some(5));
        let seen = usage.limit.unwrap();
        assert_eq!(
            (seen.status.as_str(), seen.window.as_deref(), seen.resets_at),
            ("rejected", Some("weekly"), Some(9))
        );
    }

    #[test]
    fn a_harness_is_named_by_its_wire_name_or_its_program() {
        assert_eq!(parse_harness("claude"), Some(Harness::ClaudeCode));
        assert_eq!(parse_harness("claude-code"), Some(Harness::ClaudeCode));
        assert_eq!(parse_harness("codex"), Some(Harness::Codex));
        assert_eq!(parse_harness("vim"), None);
    }
}
