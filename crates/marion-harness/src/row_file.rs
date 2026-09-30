//! **A harness row from one TOML file** (`tasks/design-b8-user-rows.md`).
//!
//! A row file states what a built-in row states, as data: the argv, the environment, the
//! declaration route and body, the no-self-update switch, the approval, the turn delivery, and the
//! rest of [`HarnessSpec`]. It is read into an owned form ([`RowDto`]), checked, and converted into
//! a [`HarnessSpec`] leaked once for the process's life, which [`crate::adapter::DataAdapter`]
//! serves exactly as it serves the built-in rows whose every rule is data. The grammar a harness's
//! stream is read by is still Rust: a file names a built-in row's (`stream = "builtin:goose"`).
//!
//! # Where rows come from
//!
//! `$XDG_CONFIG_HOME/marion/harnesses/<name>.toml` (else `~/.config/marion/harnesses/`), the
//! operator's own: loaded at startup by every marion process ([`install_user_rows`]), so a node
//! launched by the supervisor, the bridge it runs and the CLI that asked all know the same rows.
//!
//! # What is refused at load
//!
//! A file is a program marion will start, so it is held to more than a built-in row: it must be
//! owned by the user running marion and writable by no one else; its name is its stem, a row name
//! ([`marion_core::harness::valid_row_name`]), and never a built-in's or a retired harness's; it
//! states `updates`; no API key and no node token ride argv (`ps` shows argv to every user), and no
//! argv literal looks like a credential; and it passes the one validator every row does
//! ([`crate::sweep::validate`]). A refusal names the file and the key.

use std::fmt;
use std::path::{Path, PathBuf};

use marion_core::harness::Harness;
use marion_core::provider::{KeyHeader, Wire};
use serde::Deserialize;

use crate::adapter::{Row, Serve};
use crate::authority::ReadOnlyMode;
use crate::containment::ContainmentRule;
use crate::env_filter::{EnvGrant, LoginEnv};
use crate::os_sandbox::{OsSandboxRule, WritePath};
use crate::profile::{ProfileCarrier, Status};
use crate::spec::{
    self, AbortVerb, Aborts, Advertised, Approval, Arg, AxesRule, Body, Boot, BootDialog,
    BootDialogs, BootSignal, CommandLine, ConfigFile, Constraint, Deliveries, DialogAnswer, Env,
    Field, HarnessSpec, IdleSignal, KeyRecipe, LiveDeclaration, McpRoute, McpRoutes, McpServers,
    ModelForm, Modes, Need, Push, ReadOnly, Readiness, Remembers, Requirement, Resume, Spelling,
    Surfaces, TokenCarrier, TokenCarriers, ToolSpelling, TurnDelivery, UpdatePolicy, Val, When,
    WireRecipe,
};
use crate::sweep::RowFault;

/// The one schema version a row file may state.
pub const SCHEMA: u32 = 1;

/// The directory the operator's own rows live in, where one resolves.
pub fn user_dir() -> Option<PathBuf> {
    let set = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    set("XDG_CONFIG_HOME")
        .or_else(|| set("HOME").map(|h| h.join(".config")))
        .map(|c| c.join("marion").join("harnesses"))
}

/// Why a row file was not loaded: the file, then what is wrong, by key where there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowFileError {
    pub path: PathBuf,
    pub faults: Vec<String>,
}

impl fmt::Display for RowFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.faults.join("; "))
    }
}

impl std::error::Error for RowFileError {}

fn refused(path: &Path, fault: impl Into<String>) -> RowFileError {
    RowFileError {
        path: path.to_path_buf(),
        faults: vec![fault.into()],
    }
}

/// **Read, check and build the row in `path`**, under `harness`, its interned name. Nothing is
/// installed: [`install_user_rows`] and `marion harness check` both start here.
pub fn build(path: &Path, harness: Harness) -> Result<&'static HarnessSpec, RowFileError> {
    owned_by_user_alone(path)?;
    let text = std::fs::read_to_string(path).map_err(|e| refused(path, e.to_string()))?;
    build_from(path, &text, harness)
}

/// [`build`] over `text`, the file's bytes as already read — so a caller that judged those bytes
/// (a repository row's trust, by their digest) builds exactly what it judged.
pub fn build_from(
    path: &Path,
    text: &str,
    harness: Harness,
) -> Result<&'static HarnessSpec, RowFileError> {
    owned_by_user_alone(path)?;
    let dto = parse(path, text)?;
    let spec = dto.into_spec(harness).map_err(|faults| RowFileError {
        path: path.to_path_buf(),
        faults,
    })?;
    let mut faults: Vec<String> = crate::sweep::validate(spec)
        .iter()
        .map(RowFault::to_string)
        .collect();
    faults.extend(security_faults(spec));
    if faults.is_empty() {
        Ok(spec)
    } else {
        Err(RowFileError {
            path: path.to_path_buf(),
            faults,
        })
    }
}

/// The file's text as a [`RowDto`]: TOML, every key known, schema [`SCHEMA`], its name its stem.
pub fn parse(path: &Path, text: &str) -> Result<RowDto, RowFileError> {
    let dto: RowDto = toml::from_str(text).map_err(|e| refused(path, e.message().to_string()))?;
    if dto.schema != SCHEMA {
        return Err(refused(
            path,
            format!("schema: {} is not {SCHEMA}", dto.schema),
        ));
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if dto.name != stem {
        return Err(refused(
            path,
            format!("name: `{}` is not the file's stem `{stem}`", dto.name),
        ));
    }
    Ok(dto)
}

/// **A file marion will run a program from is the user's alone**: owned by the user running
/// marion, and writable by no group and no one else — or another account could rewrite what the
/// next launch starts.
fn owned_by_user_alone(path: &Path) -> Result<(), RowFileError> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(|e| refused(path, e.to_string()))?;
    // SAFETY: `geteuid` reads this process's effective uid and cannot fail.
    let me = unsafe { geteuid() };
    if meta.uid() != me {
        return Err(refused(
            path,
            "the file is not owned by the user running marion",
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(refused(
            path,
            "the file is writable by its group or by others; `chmod go-w` it",
        ));
    }
    Ok(())
}

unsafe extern "C" {
    fn geteuid() -> u32;
}

/// **What only a row read from a file is refused for**: an API key or an inline declaration on
/// argv, where `ps` shows it to every user, and an argv literal that names the node token or looks
/// like a credential.
fn security_faults(spec: &HarnessSpec) -> Vec<String> {
    let mut out = Vec::new();
    let argv = spec.argv.iter().chain(spec.pane.unwrap_or(&[]));
    for arg in argv {
        let field = match arg {
            Arg::Flag(_, f)
            | Arg::FlagEq(_, f)
            | Arg::Pair(_, _, f)
            | Arg::Each(_, f)
            | Arg::EachEq(_, f)
            | Arg::Joined(_, f)
            | Arg::Pos(f)
            | Arg::PosIfNonEmpty(f)
            | Arg::Items(f) => Some(*f),
            _ => None,
        };
        if let Some(f @ (Field::ApiKey | Field::Pairs | Field::InlineConfig)) = field {
            out.push(format!(
                "argv: {f:?} may not ride argv, which `ps` shows to every user; put it in env"
            ));
        }
        let lits: Vec<&str> = match arg {
            Arg::Lit(l) | Arg::CannedLit(l) => vec![l],
            Arg::Flag(l, _) | Arg::FlagEq(l, _) | Arg::Each(l, _) | Arg::EachEq(l, _) => vec![l],
            Arg::Pair(a, b, _) | Arg::Isolation(a, b) => vec![a, b],
            Arg::Joined(l, _) => vec![l],
            _ => vec![],
        };
        for lit in lits {
            if looks_secret(lit) {
                out.push(format!(
                    "argv: {lit:?} looks like a credential or names the node token"
                ));
            }
        }
    }
    out
}

/// A literal that names marion's node token or has a credential's shape.
fn looks_secret(s: &str) -> bool {
    s.contains(crate::mcp_bridge::NODE_TOKEN_ENV)
        || s.starts_with("sk-")
        || s.to_ascii_lowercase().starts_with("bearer ")
        || s.starts_with("ghp_")
        || s.starts_with("AKIA")
}

/// What installing the operator's rows did: the harnesses now loaded, and each file refused.
#[derive(Debug, Default)]
pub struct Installed {
    pub loaded: Vec<(Harness, PathBuf)>,
    pub refused: Vec<RowFileError>,
}

/// **Load every row in the operator's directory**, once per process, before anything parses an
/// agent type: each file is built and, if it holds, its harness loaded and its row installed. A
/// refused file is reported and skipped; the others still load. A second call installs nothing.
pub fn install_user_rows() -> &'static Installed {
    static ONCE: std::sync::OnceLock<Installed> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| user_dir().map_or_else(Installed::default, |d| install_dir(&d)))
}

/// [`install_user_rows`] over `dir`, for a test with a directory of its own.
pub fn install_dir(dir: &Path) -> Installed {
    let mut out = Installed::default();
    for path in row_files(dir) {
        let installed = std::fs::read_to_string(&path)
            .map_err(|e| refused(&path, e.to_string()))
            .and_then(|text| install_one(&path, &text, &out.loaded));
        match installed {
            Ok(h) => out.loaded.push((h, path)),
            Err(e) => out.refused.push(e),
        }
    }
    out
}

/// The `*.toml` files in `dir`, sorted; none where it does not exist.
pub fn row_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    files
}

/// **Build the row in `path` from `text` and install it**, unless a row by its name is already
/// among `loaded` (from another file: two files may not define one harness, and the refusal names
/// both) or a built-in's. The name is loaded only once its row builds, so a refused file leaves no
/// name that parses to nothing.
pub fn install_one(
    path: &Path,
    text: &str,
    loaded: &[(Harness, PathBuf)],
) -> Result<Harness, RowFileError> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if let Some((_, other)) = loaded.iter().find(|(h, _)| h.as_str() == stem) {
        return Err(refused(
            path,
            format!("name: `{stem}` is already defined by {}", other.display()),
        ));
    }
    // Refused before a byte is built where the name cannot be a row's.
    let harness = match Harness::named(stem) {
        Some(h) if !h.is_builtin() && !marion_core::harness::RETIRED.contains(&stem) => h,
        _ => {
            let why = Harness::load(stem).map_or_else(|e| e.to_string(), |_| String::new());
            return Err(refused(path, format!("name: {why}")));
        }
    };
    let spec = build_from(path, text, harness)?;
    Harness::load(stem).map_err(|e| refused(path, format!("name: {e}")))?;
    crate::adapter::install(Box::leak(Box::new(Row {
        spec,
        serve: Serve::Data,
    })));
    Ok(harness)
}

/// Where a repository's rows live: `<repo>/.marion/harnesses/`.
pub fn repo_dir(repo: &Path) -> PathBuf {
    repo.join(".marion").join("harnesses")
}

/// Whether `path` is a repository's row file: `.marion/harnesses/<name>.toml`.
pub fn is_repo_row(path: &Path) -> bool {
    path.extension().is_some_and(|x| x == "toml")
        && path
            .parent()
            .is_some_and(|d| d.ends_with(Path::new(".marion").join("harnesses")))
}

/// **What a row runs, in lines a person reads before trusting it**: the program and its argv as
/// the row writes them, the environment it sets, how a headless node is approved and how the
/// harness is kept from updating itself.
pub fn describe(spec: &HarnessSpec) -> Vec<String> {
    let argv: Vec<String> = spec.argv.iter().map(|a| format!("{a:?}")).collect();
    let env: Vec<&str> = spec.env.iter().map(|e| e.key).collect();
    vec![
        format!("program: {}", spec.program.unwrap_or("?")),
        format!("argv: {}", argv.join(" ")),
        format!("env: {}", env.join(" ")),
        format!("approval: {:?}", spec.approval),
        format!("updates: {:?}", spec.updates),
    ]
}

// ---------------------------------------------------------------------------------- leaking

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn leak_all<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

fn leak_strs(v: Vec<String>) -> &'static [&'static str] {
    leak_all(v.into_iter().map(leak).collect())
}

fn leak_opt(s: Option<String>) -> Option<&'static str> {
    s.map(leak)
}

// ---------------------------------------------------------------------------------- the file

/// A row file as TOML states it. Every key known ([`deny_unknown_fields`]), kebab-case.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RowDto {
    pub schema: u32,
    pub name: String,
    /// `cli`: a harness launched with a command line. `acp` rows are a later schema.
    pub kind: Kind,
    pub program: String,
    #[serde(default)]
    pub vendor: Option<String>,
    pub note: String,
    pub writes_without_grant: bool,
    pub surfaces: SurfacesDto,
    pub version: VersionDto,
    pub argv: Vec<ArgDto>,
    #[serde(default)]
    pub pane: Option<Vec<ArgDto>>,
    #[serde(default)]
    pub env: Vec<EnvDto>,
    /// `builtin:<row>`: the stream grammar of a built-in row.
    pub stream: String,
    /// `[verb, native]` pairs, in order.
    #[serde(default)]
    pub tools: Vec<(String, String)>,
    pub spelling: ToolSpellingDto,
    pub mcp: McpRoutesDto,
    #[serde(default)]
    pub declaration: Option<DeclarationDto>,
    #[serde(default)]
    pub files: Vec<ConfigFileDto>,
    #[serde(default)]
    pub overlay_documents: Vec<(String, String)>,
    pub login_env: LoginEnvDto,
    pub token: TokenCarriersDto,
    pub constraint: ConstraintDto,
    #[serde(default)]
    pub resume: Option<ResumeDto>,
    pub updates: UpdatePolicyDto,
    pub boot: BootDto,
    #[serde(default)]
    pub push: PushDto,
    pub approval: ApprovalDto,
    pub read_only: ReadOnlyDto,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub stderr_boilerplate: Vec<String>,
    pub delivery: DeliveriesDto,
    pub abort: AbortsDto,
    pub boot_dialogs: BootDialogsDto,
    #[serde(default)]
    pub wires: Vec<WireRecipeDto>,
    #[serde(default)]
    pub profile: Option<ProfileDto>,
    #[serde(default)]
    pub requires: Vec<RequirementDto>,
    pub axes: AxesDto,
    #[serde(default)]
    pub model: ModelFormDto,
    #[serde(default)]
    pub readiness: ReadinessDto,
    #[serde(default)]
    pub advertised: AdvertisedDto,
    pub containment: ContainmentDto,
    pub os_sandbox: OsSandboxDto,
    #[serde(default)]
    pub read_only_modes: Vec<ReadOnlyModeDto>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum Kind {
    Cli,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum SurfacesDto {
    LaunchOnly,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionDto {
    pub verified: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum FieldDto {
    Cwd,
    ConfigDir,
    Model,
    Prompt,
    Tools,
    Allowed,
    Mode,
    McpConfig,
    BaseUrl,
    BaseUrlRoot,
    ApiKey,
    Title,
    Resume,
    ProfileDir,
}

impl FieldDto {
    fn field(self) -> Field {
        match self {
            FieldDto::Cwd => Field::Cwd,
            FieldDto::ConfigDir => Field::ConfigDir,
            FieldDto::Model => Field::Model,
            FieldDto::Prompt => Field::Prompt,
            FieldDto::Tools => Field::Tools,
            FieldDto::Allowed => Field::Allowed,
            FieldDto::Mode => Field::Mode,
            FieldDto::McpConfig => Field::McpConfig,
            FieldDto::BaseUrl => Field::BaseUrl,
            FieldDto::BaseUrlRoot => Field::BaseUrlRoot,
            FieldDto::ApiKey => Field::ApiKey,
            FieldDto::Title => Field::Title,
            FieldDto::Resume => Field::Resume,
            FieldDto::ProfileDir => Field::ProfileDir,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ArgDto {
    Lit(String),
    Flag(String, FieldDto),
    FlagEq(String, FieldDto),
    Pair(String, String, FieldDto),
    Each(String, FieldDto),
    EachEq(String, FieldDto),
    Joined(String, FieldDto),
    Pos(FieldDto),
    PosIfNonEmpty(FieldDto),
    Items(FieldDto),
    Resume,
    Isolation(String, String),
    CannedLit(String),
}

impl ArgDto {
    fn arg(self) -> Arg {
        match self {
            ArgDto::Lit(l) => Arg::Lit(leak(l)),
            ArgDto::Flag(f, x) => Arg::Flag(leak(f), x.field()),
            ArgDto::FlagEq(f, x) => Arg::FlagEq(leak(f), x.field()),
            ArgDto::Pair(a, b, x) => Arg::Pair(leak(a), leak(b), x.field()),
            ArgDto::Each(f, x) => Arg::Each(leak(f), x.field()),
            ArgDto::EachEq(f, x) => Arg::EachEq(leak(f), x.field()),
            ArgDto::Joined(f, x) => Arg::Joined(leak(f), x.field()),
            ArgDto::Pos(x) => Arg::Pos(x.field()),
            ArgDto::PosIfNonEmpty(x) => Arg::PosIfNonEmpty(x.field()),
            ArgDto::Items(x) => Arg::Items(x.field()),
            ArgDto::Resume => Arg::Resume,
            ArgDto::Isolation(a, b) => Arg::Isolation(leak(a), leak(b)),
            ArgDto::CannedLit(l) => Arg::CannedLit(leak(l)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvDto {
    pub key: String,
    pub val: ValDto,
    pub when: WhenDto,
}

impl EnvDto {
    fn env(self) -> Env {
        Env {
            key: leak(self.key),
            val: self.val.val(),
            when: self.when.when(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ValDto {
    Lit(String),
    Field(FieldDto),
    Under(String),
}

impl ValDto {
    fn val(self) -> Val {
        match self {
            ValDto::Lit(l) => Val::Lit(leak(l)),
            ValDto::Field(f) => Val::Field(f.field()),
            ValDto::Under(u) => Val::Under(leak(u)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum WhenDto {
    Always,
    Overlay,
    Endpoint,
    Present(FieldDto),
}

impl WhenDto {
    fn when(self) -> When {
        match self {
            WhenDto::Always => When::Always,
            WhenDto::Overlay => When::Overlay,
            WhenDto::Endpoint => When::Endpoint,
            WhenDto::Present(f) => When::Present(f.field()),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ToolSpellingDto {
    McpDoubleUnderscore,
    ServerUnderscoreTool,
    ServerHyphenTool,
    McpDotted,
    ServerDoubleUnderscoreTool,
    ServerSlashTool,
}

impl ToolSpellingDto {
    fn spelling(self) -> ToolSpelling {
        match self {
            ToolSpellingDto::McpDoubleUnderscore => ToolSpelling::McpDoubleUnderscore,
            ToolSpellingDto::ServerUnderscoreTool => ToolSpelling::ServerUnderscoreTool,
            ToolSpellingDto::ServerHyphenTool => ToolSpelling::ServerHyphenTool,
            ToolSpellingDto::McpDotted => ToolSpelling::McpDotted,
            ToolSpellingDto::ServerDoubleUnderscoreTool => ToolSpelling::ServerDoubleUnderscoreTool,
            ToolSpellingDto::ServerSlashTool => ToolSpelling::ServerSlashTool,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpRoutesDto {
    pub canned: McpRouteDto,
    pub live: McpRouteDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum McpRouteDto {
    Document,
    Environment(String),
    Argv(String),
}

impl McpRouteDto {
    fn route(self) -> McpRoute {
        match self {
            McpRouteDto::Document => McpRoute::Document,
            McpRouteDto::Environment(k) => McpRoute::Environment(leak(k)),
            McpRouteDto::Argv(k) => McpRoute::Argv(leak(k)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum DeclarationDto {
    ArgvDocument {
        flag: String,
        file: String,
        #[serde(default)]
        prefix: String,
        body: BodyDto,
        #[serde(default)]
        always: bool,
    },
    EnvDocument {
        key: String,
        file: String,
        body: BodyDto,
    },
    EnvInline {
        key: String,
        body: BodyDto,
    },
    ArgvInline {
        flag: String,
        key: String,
        body: BodyDto,
    },
    ArgvRoot {
        flag: String,
        root: String,
        file: String,
        body: BodyDto,
    },
}

impl DeclarationDto {
    fn declaration(self) -> LiveDeclaration {
        match self {
            DeclarationDto::ArgvDocument {
                flag,
                file,
                prefix,
                body,
                always,
            } => LiveDeclaration::ArgvDocument {
                flag: leak(flag),
                file: leak(file),
                prefix: leak(prefix),
                body: body.body(),
                always,
            },
            DeclarationDto::EnvDocument { key, file, body } => LiveDeclaration::EnvDocument {
                key: leak(key),
                file: leak(file),
                body: body.body(),
            },
            DeclarationDto::EnvInline { key, body } => LiveDeclaration::EnvInline {
                key: leak(key),
                body: body.body(),
            },
            DeclarationDto::ArgvInline { flag, key, body } => LiveDeclaration::ArgvInline {
                flag: leak(flag),
                key: leak(key),
                body: body.body(),
            },
            DeclarationDto::ArgvRoot {
                flag,
                root,
                file,
                body,
            } => LiveDeclaration::ArgvRoot {
                flag: leak(flag),
                root: leak(root),
                file: leak(file),
                body: body.body(),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum BodyDto {
    McpServers(McpServersDto),
    CommandLine(CommandLineDto),
}

impl BodyDto {
    fn body(self) -> Body {
        match self {
            BodyDto::McpServers(m) => Body::McpServers(m.servers()),
            BodyDto::CommandLine(c) => Body::CommandLine(CommandLine {
                head: leak(c.head),
                split_on_whitespace: c.split_on_whitespace,
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct McpServersDto {
    #[serde(default)]
    pub typed: bool,
    #[serde(default)]
    pub nested: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub pretty: bool,
}

impl McpServersDto {
    fn servers(self) -> McpServers {
        McpServers {
            typed: self.typed,
            nested: leak_opt(self.nested),
            tools: leak_strs(self.tools),
            pretty: self.pretty,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct CommandLineDto {
    pub head: String,
    pub split_on_whitespace: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFileDto {
    pub path: String,
    pub modes: ModesDto,
    pub base: String,
    #[serde(default)]
    pub servers: Option<McpServersDto>,
    #[serde(default)]
    pub pretty: bool,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ModesDto {
    All,
    Overlay,
    Canned,
    Endpoint,
    Inherited,
}

impl ModesDto {
    fn modes(self) -> Modes {
        match self {
            ModesDto::All => Modes::All,
            ModesDto::Overlay => Modes::Overlay,
            ModesDto::Canned => Modes::Canned,
            ModesDto::Endpoint => Modes::Endpoint,
            ModesDto::Inherited => Modes::Inherited,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct LoginEnvDto {
    #[serde(default)]
    pub login: Vec<EnvGrantDto>,
    pub any_provider: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct EnvGrantDto {
    pub pattern: String,
    #[serde(default)]
    pub when_set: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenCarriersDto {
    pub canned: TokenCarrierDto,
    pub live: TokenCarrierDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum TokenCarrierDto {
    Declaration,
    InheritedEnv(String),
    ForwardedEnv(String),
    DeclaredFile(String),
}

impl TokenCarrierDto {
    fn carrier(self) -> TokenCarrier {
        match self {
            TokenCarrierDto::Declaration => TokenCarrier::Declaration,
            TokenCarrierDto::InheritedEnv(note) => TokenCarrier::InheritedEnv { note: leak(note) },
            TokenCarrierDto::ForwardedEnv(note) => TokenCarrier::ForwardedEnv { note: leak(note) },
            TokenCarrierDto::DeclaredFile(note) => TokenCarrier::DeclaredFile { note: leak(note) },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ConstraintDto {
    Allowed {
        prefix: String,
    },
    Mode {
        prefix: String,
        default: String,
        #[serde(default)]
        allowed: Option<String>,
    },
    Fixed {
        prefix: String,
        value: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ResumeDto {
    Flag(String),
    FlagEq(String),
    Subcommand(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum UpdatePolicyDto {
    Env {
        key: String,
        value: String,
        note: String,
    },
    Pair {
        key: String,
        value: String,
        note: String,
    },
    Document {
        keys: Vec<(String, bool)>,
        note: String,
    },
    Never(String),
    None(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum BootDto {
    Measured { cpu_ms: u64, note: String },
    Unmeasured(String),
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum PushDto {
    #[default]
    McpLog,
    None,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ApprovalDto {
    AllowedToolsArg {
        flag: String,
        note: String,
    },
    DeclarationKey {
        key: String,
        #[serde(default)]
        contest: Option<String>,
        note: String,
    },
    CliFlag {
        flag: String,
        scope: String,
        note: String,
    },
    EnvVar {
        key: String,
        value: String,
        scope: String,
        note: String,
    },
    OperatorAllowlist {
        file: String,
        pointer: String,
        rule: String,
        note: String,
    },
    None(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ReadOnlyDto {
    ToolsAxis {
        verified: bool,
        note: String,
    },
    Pair {
        key: String,
        value: String,
        note: String,
    },
    EnvVar {
        key: String,
        value: String,
        note: String,
    },
    ScopeOnly(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveriesDto {
    pub headless: TurnDeliveryDto,
    pub interactive: TurnDeliveryDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum TurnDeliveryDto {
    Continuation(String),
    TerminalPaste {
        idle_quiet_ms: u32,
        boot: BootSignalDto,
        submit: String,
        submit_delay_ms: u16,
        note: String,
    },
    None(String),
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum BootSignalDto {
    FirstDraw,
    WindowTitle,
}

impl TurnDeliveryDto {
    fn delivery(self) -> TurnDelivery {
        match self {
            TurnDeliveryDto::Continuation(note) => TurnDelivery::Continuation { note: leak(note) },
            TurnDeliveryDto::TerminalPaste {
                idle_quiet_ms,
                boot,
                submit,
                submit_delay_ms,
                note,
            } => TurnDelivery::TerminalPaste {
                idle: IdleSignal::OutputQuiet { ms: idle_quiet_ms },
                boot: match boot {
                    BootSignalDto::FirstDraw => BootSignal::FirstDraw,
                    BootSignalDto::WindowTitle => BootSignal::WindowTitle,
                },
                submit: leak(submit).as_bytes(),
                submit_delay_ms,
                note: leak(note),
            },
            TurnDeliveryDto::None(note) => TurnDelivery::None { note: leak(note) },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbortsDto {
    pub headless: AbortVerbDto,
    pub interactive: AbortVerbDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum AbortVerbDto {
    Keys {
        keys: Vec<String>,
        gap_ms: u16,
        grace_ms: u32,
        note: String,
    },
    None(String),
}

impl AbortVerbDto {
    fn verb(self) -> AbortVerb {
        match self {
            AbortVerbDto::Keys {
                keys,
                gap_ms,
                grace_ms,
                note,
            } => AbortVerb::Keys {
                keys: leak_all(keys.into_iter().map(|k| leak(k).as_bytes()).collect()),
                gap_ms,
                grace_ms,
                note: leak(note),
            },
            AbortVerbDto::None(note) => AbortVerb::None { note: leak(note) },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootDialogsDto {
    #[serde(default)]
    pub dialogs: Vec<BootDialogDto>,
    pub remembers: RemembersDto,
    pub note: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootDialogDto {
    pub needle: String,
    pub action: String,
    pub answer: DialogAnswerDto,
    pub note: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum DialogAnswerDto {
    Keys(String),
    Hold,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum RemembersDto {
    Nothing,
    CannedHome,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRecipeDto {
    pub wire: Wire,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub keys: Vec<KeyRecipeDto>,
    pub note: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum KeyRecipeDto {
    /// [`spec::BEARER_BY_OVERLAY`]: the row's own overlay sends the key as a bearer token.
    BearerByOverlay,
    Header {
        header: KeyHeaderDto,
        env: Vec<EnvDto>,
        note: String,
    },
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum KeyHeaderDto {
    Bearer,
    XApiKey,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProfileDto {
    pub env: String,
    #[serde(default)]
    pub clear: Vec<String>,
    pub status: ProfileStatusDto,
    #[serde(default)]
    pub login_hint: String,
    pub home_default: String,
    #[serde(default)]
    pub shared: Vec<String>,
    pub note: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ProfileStatusDto {
    JsonBool { argv: Vec<String>, key: String },
    TextAbsent { argv: Vec<String>, text: String },
    FileExists(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementDto {
    pub modes: ModesDto,
    pub need: NeedDto,
    pub why: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum NeedDto {
    BaseUrl,
    Model,
    ApiKey,
    ModelOtherThan(String),
    NoRecipe,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum AxesDto {
    Split,
    Mode {
        mode: String,
        when_any: Vec<String>,
        #[serde(default)]
        by_name: Vec<String>,
    },
    OneList {
        #[serde(default)]
        refuse_empty: Option<String>,
    },
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ModelFormDto {
    #[default]
    AsGiven,
    OmitUnderCanned,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ReadinessDto {
    #[default]
    Ungated,
    Marker {
        prompt: String,
        marker: String,
    },
}

/// Capabilities are measured per surface; a row file claims none (§3.3's static set applies).
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum AdvertisedDto {
    #[default]
    None,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum ContainmentDto {
    ToolsOnly,
    HarnessSandbox { verify: Vec<String> },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum OsSandboxDto {
    Wrap { writes: Vec<WritePathDto> },
    Unsupported(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum WritePathDto {
    Home(String),
    HomeProject(String),
    TmpUserProject(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadOnlyModeDto {
    pub flags: Vec<String>,
    pub values: Vec<String>,
}

impl RowDto {
    /// **The row as a [`HarnessSpec`]**, leaked for the process's life, under `harness`. The
    /// faults are the ones only the file's form can have — an unknown built-in grammar, a kind
    /// this schema has no row for; everything else is [`crate::sweep::validate`]'s.
    pub fn into_spec(self, harness: Harness) -> Result<&'static HarnessSpec, Vec<String>> {
        let mut faults = Vec::new();
        if self.kind != Kind::Cli {
            faults.push("kind: only `cli` rows load from a file in this schema".to_string());
        }
        let stream = match self.stream.strip_prefix("builtin:") {
            Some(name) => Harness::ALL
                .into_iter()
                .find(|h| h.as_str() == name || h.cli_name() == name)
                .and_then(|h| crate::adapter::harness_spec(h).stream),
            None => None,
        };
        if stream.is_none() {
            faults.push(format!(
                "stream: {:?} names no built-in row's grammar (`builtin:<row>`)",
                self.stream
            ));
        }
        if !faults.is_empty() {
            return Err(faults);
        }
        let SurfacesDto::LaunchOnly = self.surfaces;
        let spec = HarnessSpec {
            login_env: LoginEnv {
                login: leak_all(
                    self.login_env
                        .login
                        .into_iter()
                        .map(|g| EnvGrant {
                            pattern: leak(g.pattern),
                            when_set: leak_opt(g.when_set),
                        })
                        .collect(),
                ),
                any_provider: self.login_env.any_provider,
            },
            harness,
            surfaces: Surfaces::LaunchOnly,
            program: Some(leak(self.program)),
            vendor: leak_opt(self.vendor),
            verified: leak_strs(self.version.verified),
            argv: leak_all(self.argv.into_iter().map(ArgDto::arg).collect()),
            pane: self
                .pane
                .map(|p| leak_all(p.into_iter().map(ArgDto::arg).collect())),
            env: leak_all(self.env.into_iter().map(EnvDto::env).collect()),
            stream,
            tool_names: leak_all(
                self.tools
                    .into_iter()
                    .map(|(v, n)| (leak(v), leak(n)))
                    .collect(),
            ),
            spelling: Spelling::Fixed(self.spelling.spelling()),
            mcp: McpRoutes {
                canned: self.mcp.canned.route(),
                live: self.mcp.live.route(),
            },
            live_declaration: self.declaration.map(DeclarationDto::declaration),
            token: TokenCarriers {
                canned: self.token.canned.carrier(),
                live: self.token.live.carrier(),
            },
            constraint: match self.constraint {
                ConstraintDto::Allowed { prefix } => Constraint::Allowed {
                    prefix: leak(prefix),
                },
                ConstraintDto::Mode {
                    prefix,
                    default,
                    allowed,
                } => Constraint::Mode {
                    prefix: leak(prefix),
                    default: leak(default),
                    allowed: leak_opt(allowed),
                },
                ConstraintDto::Fixed { prefix, value } => Constraint::Fixed {
                    prefix: leak(prefix),
                    value: leak(value),
                },
            },
            resume: self.resume.map(|r| match r {
                ResumeDto::Flag(f) => Resume::Flag(leak(f)),
                ResumeDto::FlagEq(f) => Resume::FlagEq(leak(f)),
                ResumeDto::Subcommand(f) => Resume::Subcommand(leak(f)),
            }),
            updates: match self.updates {
                UpdatePolicyDto::Env { key, value, note } => UpdatePolicy::Env {
                    key: leak(key),
                    value: leak(value),
                    note: leak(note),
                },
                UpdatePolicyDto::Pair { key, value, note } => UpdatePolicy::Pair {
                    key: leak(key),
                    value: leak(value),
                    note: leak(note),
                },
                UpdatePolicyDto::Document { keys, note } => UpdatePolicy::Document {
                    keys: leak_all(keys.into_iter().map(|(k, b)| (leak(k), b)).collect()),
                    note: leak(note),
                },
                UpdatePolicyDto::Never(note) => UpdatePolicy::Never { note: leak(note) },
                UpdatePolicyDto::None(note) => UpdatePolicy::None { note: leak(note) },
            },
            boot: match self.boot {
                BootDto::Measured { cpu_ms, note } => Boot::Measured {
                    cpu: std::time::Duration::from_millis(cpu_ms),
                    note: leak(note),
                },
                BootDto::Unmeasured(note) => Boot::Unmeasured { note: leak(note) },
            },
            push: match self.push {
                PushDto::McpLog => Push::McpLog,
                PushDto::None => Push::None,
            },
            approval: match self.approval {
                ApprovalDto::AllowedToolsArg { flag, note } => Approval::AllowedToolsArg {
                    flag: leak(flag),
                    note: leak(note),
                },
                ApprovalDto::DeclarationKey { key, contest, note } => Approval::DeclarationKey {
                    key: leak(key),
                    contest: leak_opt(contest),
                    note: leak(note),
                },
                ApprovalDto::CliFlag { flag, scope, note } => Approval::CliFlag {
                    flag: leak(flag),
                    scope: leak(scope),
                    note: leak(note),
                },
                ApprovalDto::EnvVar {
                    key,
                    value,
                    scope,
                    note,
                } => Approval::EnvVar {
                    key: leak(key),
                    value: leak(value),
                    scope: leak(scope),
                    note: leak(note),
                },
                ApprovalDto::OperatorAllowlist {
                    file,
                    pointer,
                    rule,
                    note,
                } => Approval::OperatorAllowlist {
                    file: leak(file),
                    pointer: leak(pointer),
                    rule: leak(rule),
                    note: leak(note),
                },
                ApprovalDto::None(note) => Approval::None { note: leak(note) },
            },
            read_only: match self.read_only {
                ReadOnlyDto::ToolsAxis { verified, note } => ReadOnly::ToolsAxis {
                    verified,
                    note: leak(note),
                },
                ReadOnlyDto::Pair { key, value, note } => ReadOnly::Pair {
                    key: leak(key),
                    value: leak(value),
                    note: leak(note),
                },
                ReadOnlyDto::EnvVar { key, value, note } => ReadOnly::EnvVar {
                    key: leak(key),
                    value: leak(value),
                    note: leak(note),
                },
                ReadOnlyDto::ScopeOnly(note) => ReadOnly::ScopeOnly { note: leak(note) },
            },
            client_name: leak_opt(self.client_name),
            stderr_boilerplate: leak_strs(self.stderr_boilerplate),
            delivery: Deliveries {
                headless: self.delivery.headless.delivery(),
                interactive: self.delivery.interactive.delivery(),
            },
            abort: Aborts {
                headless: self.abort.headless.verb(),
                interactive: self.abort.interactive.verb(),
            },
            boot_dialogs: BootDialogs {
                dialogs: leak_all(
                    self.boot_dialogs
                        .dialogs
                        .into_iter()
                        .map(|d| BootDialog {
                            needle: leak(d.needle),
                            action: leak(d.action),
                            answer: match d.answer {
                                DialogAnswerDto::Keys(k) => DialogAnswer::Keys(leak(k).as_bytes()),
                                DialogAnswerDto::Hold => DialogAnswer::Hold,
                            },
                            note: leak(d.note),
                        })
                        .collect(),
                ),
                remembers: match self.boot_dialogs.remembers {
                    RemembersDto::Nothing => Remembers::Nothing,
                    RemembersDto::CannedHome => Remembers::CannedHome,
                },
                note: leak(self.boot_dialogs.note),
            },
            wires: leak_all(
                self.wires
                    .into_iter()
                    .map(|w| WireRecipe {
                        wire: w.wire,
                        env: leak_all(w.env.into_iter().map(|(k, v)| (leak(k), leak(v))).collect()),
                        keys: leak_all(
                            w.keys
                                .into_iter()
                                .map(|k| match k {
                                    KeyRecipeDto::BearerByOverlay => spec::BEARER_BY_OVERLAY,
                                    KeyRecipeDto::Header { header, env, note } => KeyRecipe {
                                        header: match header {
                                            KeyHeaderDto::Bearer => KeyHeader::Bearer,
                                            KeyHeaderDto::XApiKey => KeyHeader::XApiKey,
                                        },
                                        env: leak_all(env.into_iter().map(EnvDto::env).collect()),
                                        note: leak(note),
                                    },
                                })
                                .collect(),
                        ),
                        note: leak(w.note),
                    })
                    .collect(),
            ),
            profile: self.profile.map(|p| ProfileCarrier {
                env: leak(p.env),
                clear: leak_strs(p.clear),
                status: match p.status {
                    ProfileStatusDto::JsonBool { argv, key } => Status::JsonBool {
                        argv: leak_strs(argv),
                        key: leak(key),
                    },
                    ProfileStatusDto::TextAbsent { argv, text } => Status::TextAbsent {
                        argv: leak_strs(argv),
                        text: leak(text),
                    },
                    ProfileStatusDto::FileExists(f) => Status::FileExists(leak(f)),
                },
                login_hint: leak(p.login_hint),
                home_default: leak(p.home_default),
                shared: leak_strs(p.shared),
                note: leak(p.note),
            }),
            note: leak(self.note),
            requires: leak_all(
                self.requires
                    .into_iter()
                    .map(|r| Requirement {
                        modes: r.modes.modes(),
                        need: match r.need {
                            NeedDto::BaseUrl => Need::BaseUrl,
                            NeedDto::Model => Need::Model,
                            NeedDto::ApiKey => Need::ApiKey,
                            NeedDto::ModelOtherThan(m) => Need::ModelOtherThan(leak(m)),
                            NeedDto::NoRecipe => Need::NoRecipe,
                        },
                        why: leak(r.why),
                    })
                    .collect(),
            ),
            axes: match self.axes {
                AxesDto::Split => AxesRule::Split,
                AxesDto::Mode {
                    mode,
                    when_any,
                    by_name,
                } => AxesRule::Mode {
                    mode: leak(mode),
                    when_any: leak_strs(when_any),
                    by_name: leak_strs(by_name),
                },
                AxesDto::OneList { refuse_empty } => AxesRule::OneList {
                    refuse_empty: leak_opt(refuse_empty),
                },
            },
            files: leak_all(
                self.files
                    .into_iter()
                    .map(|f| ConfigFile {
                        path: leak(f.path),
                        modes: f.modes.modes(),
                        base: leak(f.base),
                        servers: f.servers.map(McpServersDto::servers),
                        pretty: f.pretty,
                    })
                    .collect(),
            ),
            overlay_documents: leak_all(
                self.overlay_documents
                    .into_iter()
                    .map(|(p, b)| (leak(p), leak(b)))
                    .collect(),
            ),
            model: match self.model {
                ModelFormDto::AsGiven => ModelForm::AsGiven,
                ModelFormDto::OmitUnderCanned => ModelForm::OmitUnderCanned,
            },
            readiness: match self.readiness {
                ReadinessDto::Ungated => Readiness::Ungated,
                ReadinessDto::Marker { prompt, marker } => Readiness::Marker {
                    prompt: leak(prompt),
                    marker: leak(marker),
                },
            },
            advertised: match self.advertised {
                AdvertisedDto::None => Advertised::NONE,
            },
            writes_without_grant: self.writes_without_grant,
            containment: match self.containment {
                ContainmentDto::ToolsOnly => ContainmentRule::ToolsOnly,
                ContainmentDto::HarnessSandbox { verify } => ContainmentRule::HarnessSandbox {
                    verify: leak_strs(verify),
                },
            },
            os_sandbox: match self.os_sandbox {
                OsSandboxDto::Wrap { writes } => OsSandboxRule::Wrap {
                    writes: leak_all(
                        writes
                            .into_iter()
                            .map(|w| match w {
                                WritePathDto::Home(p) => WritePath::Home(leak(p)),
                                WritePathDto::HomeProject(p) => WritePath::HomeProject(leak(p)),
                                WritePathDto::TmpUserProject(p) => {
                                    WritePath::TmpUserProject(leak(p))
                                }
                            })
                            .collect(),
                    ),
                },
                OsSandboxDto::Unsupported(why) => OsSandboxRule::Unsupported { why: leak(why) },
            },
            read_only_modes: leak_all(
                self.read_only_modes
                    .into_iter()
                    .map(|m| ReadOnlyMode {
                        flags: leak_strs(m.flags),
                        values: leak_strs(m.values),
                    })
                    .collect(),
            ),
        };
        Ok(Box::leak(Box::new(spec)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::harness_spec;

    const QWEN_TWIN: &str = include_str!("../tests/fixtures/rows/qwen-twin.toml");
    const GOOSE_TWIN: &str = include_str!("../tests/fixtures/rows/goose-twin.toml");

    /// `text` as a row file named `name`, the user's alone, in a scratch dir of its own.
    fn file(tag: &str, name: &str, text: &str) -> (marion_testsupport::Scratch, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = marion_testsupport::scratch(tag);
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, path)
    }

    /// The first line two long renderings differ on, for a failure a person can read.
    fn first_difference(a: &str, b: &str) -> String {
        a.lines()
            .zip(b.lines())
            .enumerate()
            .find(|(_, (x, y))| x != y)
            .map_or_else(
                || "(one is a prefix of the other)".into(),
                |(i, (x, y))| format!("line {i}:\n  file:     {x}\n  built-in: {y}"),
            )
    }

    /// **The TOML twins of qwen and goose are their built-in rows, byte for byte**: every field of
    /// the row a file builds — argv, env, declaration and its body, documents, token carriers,
    /// update policy, approval, delivery, notes — equals the built-in's, down to each note's text.
    /// Both built-ins are served by their data alone ([`crate::adapter::DataAdapter`]), so equal
    /// rows are equal launches.
    #[test]
    fn the_toml_twins_of_qwen_and_goose_are_their_built_in_rows_byte_for_byte() {
        for (name, text, built_in) in [
            ("qwen-twin", QWEN_TWIN, Harness::Qwen),
            ("goose-twin", GOOSE_TWIN, Harness::Goose),
        ] {
            let (_dir, path) = file(&format!("row-twin-{name}"), name, text);
            let h = Harness::named(name).unwrap();
            let spec = build(&path, h).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(spec.harness, h);
            let as_built_in = HarnessSpec {
                harness: built_in,
                ..*spec
            };
            let ours = format!("{as_built_in:#?}");
            let theirs = format!("{:#?}", harness_spec(built_in));
            assert!(
                ours == theirs,
                "{name}: {}",
                first_difference(&ours, &theirs)
            );
        }
    }

    /// Replace `from` with `to` in the qwen twin, and build the result as `name`.
    fn qwen_variant(
        tag: &str,
        name: &str,
        from: &str,
        to: &str,
    ) -> Result<&'static HarnessSpec, RowFileError> {
        assert!(QWEN_TWIN.contains(from), "{from}");
        let text = QWEN_TWIN
            .replacen(from, to, 1)
            .replace("name = \"qwen-twin\"", &format!("name = \"{name}\""));
        let (_dir, path) = file(tag, name, &text);
        build(&path, Harness::named(name).unwrap())
    }

    /// **What a row file is refused for at load, by key**: a node token or an API key on argv, a
    /// missing update policy, a grammar no built-in has, a name that is not the file's stem.
    #[test]
    fn a_row_file_is_refused_for_what_would_leak_or_drift() {
        let cases: [(&str, &str, &str, &str); 4] = [
            (
                "token-argv",
                r#"{ lit = "--yolo" },"#,
                r#"{ lit = "MARION_NODE_TOKEN=x" },"#,
                "names the node token",
            ),
            (
                "key-argv",
                r#"{ lit = "--yolo" },"#,
                r#"{ flag = ["--key", "api-key"] },"#,
                "may not ride argv",
            ),
            (
                "no-updates",
                "[updates.env]",
                "[not-updates.env]",
                "not-updates",
            ),
            (
                "grammar",
                "stream = \"builtin:qwen\"",
                "stream = \"builtin:nothing\"",
                "names no built-in row's grammar",
            ),
        ];
        for (tag, from, to, want) in cases {
            let name = format!("tb8-{tag}");
            let err = qwen_variant(&format!("row-refuse-{tag}"), &name, from, to)
                .expect_err(tag)
                .to_string();
            assert!(err.contains(want), "{tag}: {err}");
        }
        let (_dir, path) = file("row-refuse-stem", "tb8-stem", QWEN_TWIN);
        let err = build(&path, Harness::named("tb8-stem").unwrap()).unwrap_err();
        assert!(err.to_string().contains("is not the file's stem"), "{err}");
    }

    /// **An argv declaration that carries the node token is refused** under the key it breaks:
    /// the live declaration rides `--mcp-config`, so its carrier must withhold the token.
    #[test]
    fn an_argv_declaration_that_carries_the_node_token_is_refused() {
        // The whole `live` entry, its note a multi-line string, up to the end of `[token]`.
        let at = QWEN_TWIN.find("live = { inherited-env =").unwrap();
        let end = at + QWEN_TWIN[at..].find("\n\n").unwrap();
        let from = &QWEN_TWIN[at..end];
        let err = qwen_variant(
            "row-refuse-carrier",
            "tb8-carrier",
            from,
            "live = \"declaration\"",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("mcp.live: the live declaration rides argv and carries the node token"),
            "{err}"
        );
    }

    /// **A file another user could rewrite is not run from**: group- or world-writable is refused
    /// before a byte is parsed.
    #[test]
    fn a_row_file_writable_by_others_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = file("row-refuse-mode", "tb8-mode", QWEN_TWIN);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622)).unwrap();
        let err = build(&path, Harness::named("tb8-mode").unwrap()).unwrap_err();
        assert!(
            err.to_string()
                .contains("writable by its group or by others"),
            "{err}"
        );
    }

    /// **Installing a directory loads what holds and reports what does not**: a twin loads and
    /// launches, a file named for a built-in is refused for shadowing, and the rest still load.
    #[test]
    fn installing_a_directory_loads_what_holds_and_refuses_a_shadow() {
        use std::os::unix::fs::PermissionsExt;
        let dir = marion_testsupport::scratch("row-install");
        for name in ["tb8-installed", "codex"] {
            let path = dir.join(format!("{name}.toml"));
            let text = QWEN_TWIN.replace("name = \"qwen-twin\"", &format!("name = \"{name}\""));
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let out = install_dir(&dir);
        assert_eq!(out.loaded.len(), 1, "{:?}", out.refused);
        let (h, _) = &out.loaded[0];
        assert_eq!(h.as_str(), "tb8-installed");
        assert!(h.is_loaded() && crate::adapter::every().contains(h));
        assert!(crate::adapter::adapter_for(*h).is_ok());
        assert_eq!(out.refused.len(), 1);
        assert!(
            out.refused[0].to_string().contains("marion ships"),
            "{}",
            out.refused[0]
        );
    }
}
