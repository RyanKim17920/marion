use std::collections::HashMap;

use crate::agent_type::{self, AgentType};

/// The exact command words owned by marion rather than a native facade.
const RESERVED_COMMANDS: &[&str] = &["help", "doctor", "version", "run", "attach", "tree", "mcp"];

/// A named facade with independently configured native and structured lanes.
///
/// This is metadata only; it neither locates nor launches a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFacadeDescriptor {
    pub identity: VendorIdentity,
    pub command: &'static str,
    pub aliases: &'static [&'static str],
    pub native: Option<NativeFacadeNativeLane>,
    pub structured: Option<NativeFacadeStructuredLane>,
}

/// Stable vendor identity owned by Marion rather than inferred from an executable basename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorIdentity(&'static str);

impl VendorIdentity {
    pub const fn new(identity: &'static str) -> Self {
        Self(identity)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Stable identifier for a native injection/readiness adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeAdapterId(&'static str);

impl NativeAdapterId {
    pub const fn new(identity: &'static str) -> Self {
        Self(identity)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Stable identifier for a structured protocol adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuredAdapterId(&'static str);

impl StructuredAdapterId {
    pub const fn new(identity: &'static str) -> Self {
        Self(identity)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Static enablement policy paired with one lane's immutable configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lane<T> {
    enabled: bool,
    config: T,
}

impl<T> Lane<T> {
    pub const fn new(enabled: bool, config: T) -> Self {
        Self { enabled, config }
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub const fn config(&self) -> &T {
        &self.config
    }

    /// Requires current computed evidence as well as static enablement policy.
    pub const fn is_bindable(&self, readiness: LaneReadiness) -> bool {
        self.enabled && readiness.is_ready()
    }
}

/// Immutable native-lane configuration. Runtime readiness never lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeLane {
    executable: &'static str,
    agent_type: &'static str,
    adapter: NativeAdapterId,
}

impl NativeLane {
    pub const fn new(
        executable: &'static str,
        agent_type: &'static str,
        adapter: NativeAdapterId,
    ) -> Self {
        Self {
            executable,
            agent_type,
            adapter,
        }
    }

    pub const fn executable(&self) -> &'static str {
        self.executable
    }

    pub const fn agent_type_name(&self) -> &'static str {
        self.agent_type
    }

    pub const fn adapter(&self) -> NativeAdapterId {
        self.adapter
    }
}

/// One concrete structured agent implementation and protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuredAgentIdentity {
    name: &'static str,
    protocol_version: u16,
}

impl StructuredAgentIdentity {
    pub const fn new(name: &'static str, protocol_version: u16) -> Self {
        Self {
            name,
            protocol_version,
        }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }

    pub const fn protocol_version(self) -> u16 {
        self.protocol_version
    }
}

/// The control plane used by a concrete structured agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredControl {
    Typed,
    Acp,
    Mcp,
}

/// Immutable structured-lane configuration. Runtime readiness never lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuredLane {
    agent_identity: StructuredAgentIdentity,
    control: StructuredControl,
    adapter: StructuredAdapterId,
}

impl StructuredLane {
    pub const fn new(
        agent_identity: StructuredAgentIdentity,
        control: StructuredControl,
        adapter: StructuredAdapterId,
    ) -> Self {
        Self {
            agent_identity,
            control,
            adapter,
        }
    }

    pub const fn agent_identity(&self) -> StructuredAgentIdentity {
        self.agent_identity
    }

    pub const fn control(&self) -> StructuredControl {
        self.control
    }

    pub const fn adapter(&self) -> StructuredAdapterId {
        self.adapter
    }
}

pub type NativeFacadeNativeLane = Lane<NativeLane>;
pub type NativeFacadeStructuredLane = Lane<StructuredLane>;
pub type NativeFacadeStructuredTransport = StructuredControl;

impl Lane<NativeLane> {
    pub const fn executable(&self) -> &'static str {
        self.config.executable()
    }

    pub const fn agent_type_name(&self) -> &'static str {
        self.config.agent_type_name()
    }

    pub const fn adapter(&self) -> NativeAdapterId {
        self.config.adapter()
    }
}

impl Lane<StructuredLane> {
    pub const fn agent_identity(&self) -> StructuredAgentIdentity {
        self.config.agent_identity()
    }

    pub const fn control(&self) -> StructuredControl {
        self.config.control()
    }

    pub const fn transport(&self) -> StructuredControl {
        self.config.control()
    }

    pub const fn adapter(&self) -> StructuredAdapterId {
        self.config.adapter()
    }
}

/// The computed result for one lane at one doctor or bind evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneReadinessResult {
    Disabled,
    Blocked(LaneReadinessBlock),
    Ready,
}

/// The first causal fact that blocks one evaluated lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneReadinessBlock {
    NotInstalled,
    VersionIncompatible,
    InjectionUnavailable,
    BehaviorUnavailable,
}

/// Current readiness facts. These are computed evidence, never vendor declaration state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneReadiness {
    enabled: bool,
    installed: bool,
    version_compatible: bool,
    injection_ready: bool,
    behavior_ready: bool,
    result: LaneReadinessResult,
}

impl LaneReadiness {
    /// Derive one current report from observed facts in stable causal order.
    pub const fn evaluate(
        enabled: bool,
        installed: bool,
        version_compatible: bool,
        injection_ready: bool,
        behavior_ready: bool,
    ) -> Self {
        let result = if !enabled {
            LaneReadinessResult::Disabled
        } else if !installed {
            LaneReadinessResult::Blocked(LaneReadinessBlock::NotInstalled)
        } else if !version_compatible {
            LaneReadinessResult::Blocked(LaneReadinessBlock::VersionIncompatible)
        } else if !injection_ready {
            LaneReadinessResult::Blocked(LaneReadinessBlock::InjectionUnavailable)
        } else if !behavior_ready {
            LaneReadinessResult::Blocked(LaneReadinessBlock::BehaviorUnavailable)
        } else {
            LaneReadinessResult::Ready
        };
        Self {
            enabled,
            installed,
            version_compatible,
            injection_ready,
            behavior_ready,
            result,
        }
    }

    pub const fn enabled(self) -> bool {
        self.enabled
    }

    pub const fn installed(self) -> bool {
        self.installed
    }

    pub const fn version_compatible(self) -> bool {
        self.version_compatible
    }

    pub const fn injection_ready(self) -> bool {
        self.injection_ready
    }

    pub const fn behavior_ready(self) -> bool {
        self.behavior_ready
    }

    pub const fn is_ready(self) -> bool {
        self.enabled
            && self.installed
            && self.version_compatible
            && self.injection_ready
            && self.behavior_ready
            && matches!(self.result, LaneReadinessResult::Ready)
    }

    pub const fn result(self) -> LaneReadinessResult {
        self.result
    }
}

/// The lane selected by a provenance-bearing launch intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFacadeLaunchMode {
    Native,
    Structured,
}

/// The reason a command token cannot be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeFacadeTokenError {
    #[error("is empty")]
    Empty,
    #[error("contains non-ASCII characters")]
    NonAscii,
    #[error("starts with '-'")]
    LeadingDash,
}

/// The reason an executable program name cannot be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeFacadeExecutableError {
    #[error("is empty")]
    Empty,
    #[error("contains non-ASCII characters")]
    NonAscii,
    #[error("starts with '-'")]
    LeadingDash,
    #[error("contains a path separator")]
    PathSeparator,
}

/// A malformed or ambiguous native-facade descriptor set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeFacadeValidationError {
    #[error("invalid vendor identity {identity:?}: {reason}")]
    InvalidVendorIdentity {
        identity: &'static str,
        reason: NativeFacadeTokenError,
    },
    #[error("vendor identity {identity:?} is registered more than once")]
    DuplicateVendorIdentity { identity: &'static str },
    #[error("invalid native facade command {command:?}: {reason}")]
    InvalidCommand {
        command: &'static str,
        reason: NativeFacadeTokenError,
    },
    #[error("invalid alias {alias:?} for native facade command {command:?}: {reason}")]
    InvalidAlias {
        command: &'static str,
        alias: &'static str,
        reason: NativeFacadeTokenError,
    },
    #[error("invalid executable {executable:?} for native facade command {command:?}: {reason}")]
    InvalidExecutable {
        command: &'static str,
        executable: &'static str,
        reason: NativeFacadeExecutableError,
    },
    #[error("native facade command {command:?} is reserved")]
    ReservedCommand { command: &'static str },
    #[error("native facade alias {alias:?} for {command:?} is reserved")]
    ReservedAlias {
        command: &'static str,
        alias: &'static str,
    },
    #[error("native facade command {command:?} is registered more than once")]
    DuplicatePrimary { command: &'static str },
    #[error("unknown native agent type {agent_type:?} for facade command {command:?}")]
    UnknownNativeAgentType {
        command: &'static str,
        agent_type: &'static str,
    },
    #[error("invalid native adapter id {adapter:?} for facade command {command:?}: {reason}")]
    InvalidNativeAdapterId {
        command: &'static str,
        adapter: &'static str,
        reason: NativeFacadeTokenError,
    },
    #[error(
        "invalid structured agent identity {identity:?} for facade command {command:?}: {reason}"
    )]
    InvalidStructuredAgentIdentity {
        command: &'static str,
        identity: &'static str,
        reason: NativeFacadeTokenError,
    },
    #[error("structured agent identity for facade command {command:?} has protocol version zero")]
    InvalidStructuredProtocolVersion { command: &'static str },
    #[error("invalid structured adapter id {adapter:?} for facade command {command:?}: {reason}")]
    InvalidStructuredAdapterId {
        command: &'static str,
        adapter: &'static str,
        reason: NativeFacadeTokenError,
    },
    #[error(
        "native facade alias {alias:?} for command {alias_command:?} collides with primary command {primary_command:?}"
    )]
    AliasMatchesPrimary {
        alias: &'static str,
        alias_command: &'static str,
        primary_command: &'static str,
    },
    #[error(
        "native facade alias {alias:?} is registered for both {first_command:?} and {second_command:?}"
    )]
    DuplicateAlias {
        alias: &'static str,
        first_command: &'static str,
        second_command: &'static str,
    },
}

/// A validated borrowed descriptor collection.
///
/// Resolution is deliberately a name lookup over these descriptors only. In particular, it never
/// reads `PATH`, probes the filesystem, installs a program, or derives a program path.
#[derive(Debug)]
pub struct NativeFacadeRegistry<'a> {
    entries: Vec<RegisteredNativeFacade<'a>>,
}

#[derive(Debug)]
struct RegisteredNativeFacade<'a> {
    descriptor: &'a NativeFacadeDescriptor,
    native_agent_type: Option<AgentType>,
}

/// One descriptor resolved through a validated registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedNativeFacade<'a> {
    descriptor: &'a NativeFacadeDescriptor,
    native_agent_type: Option<&'a AgentType>,
}

impl<'a> ResolvedNativeFacade<'a> {
    pub const fn descriptor(self) -> &'a NativeFacadeDescriptor {
        self.descriptor
    }

    pub fn native_lane(self) -> Option<ResolvedNativeFacadeNativeLane<'a>> {
        Some(ResolvedNativeFacadeNativeLane {
            lane: self.descriptor.native.as_ref()?,
            agent_type: self.native_agent_type?,
        })
    }

    pub const fn structured_lane(self) -> Option<&'a NativeFacadeStructuredLane> {
        self.descriptor.structured.as_ref()
    }
}

/// A native lane whose agent-type identity was canonicalized during registry construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedNativeFacadeNativeLane<'a> {
    lane: &'a NativeFacadeNativeLane,
    agent_type: &'a AgentType,
}

impl<'a> ResolvedNativeFacadeNativeLane<'a> {
    pub const fn lane(self) -> &'a NativeFacadeNativeLane {
        self.lane
    }

    pub const fn is_bindable(self, readiness: LaneReadiness) -> bool {
        self.lane.is_bindable(readiness)
    }

    pub const fn executable(self) -> &'static str {
        self.lane.executable()
    }

    pub const fn agent_type(self) -> &'a AgentType {
        self.agent_type
    }
}

impl<'a> NativeFacadeRegistry<'a> {
    /// Validates every spelling in one registration pass before exposing the descriptors.
    pub fn new(
        descriptors: &'a [NativeFacadeDescriptor],
    ) -> Result<Self, NativeFacadeValidationError> {
        let mut primaries = HashMap::with_capacity(descriptors.len());
        let mut vendor_identities = HashMap::with_capacity(descriptors.len());
        let alias_capacity = descriptors
            .iter()
            .map(|descriptor| descriptor.aliases.len())
            .sum();
        let mut aliases = HashMap::with_capacity(alias_capacity);
        let mut entries = Vec::with_capacity(descriptors.len());

        for descriptor in descriptors {
            validate_vendor_identity(descriptor.identity)?;
            validate_command(descriptor.command)?;
            let native_agent_type = if let Some(native) = descriptor.native {
                validate_executable(descriptor.command, native.executable())?;
                validate_native_adapter(descriptor.command, native.adapter())?;
                Some(agent_type::builtin(native.agent_type_name()).ok_or(
                    NativeFacadeValidationError::UnknownNativeAgentType {
                        command: descriptor.command,
                        agent_type: native.agent_type_name(),
                    },
                )?)
            } else {
                None
            };
            if let Some(structured) = descriptor.structured {
                validate_structured_identity(descriptor.command, structured.agent_identity())?;
                validate_structured_adapter(descriptor.command, structured.adapter())?;
            }
            if RESERVED_COMMANDS.contains(&descriptor.command) {
                return Err(NativeFacadeValidationError::ReservedCommand {
                    command: descriptor.command,
                });
            }
            if primaries.insert(descriptor.command, ()).is_some() {
                return Err(NativeFacadeValidationError::DuplicatePrimary {
                    command: descriptor.command,
                });
            }
            if let Some(&alias_command) = aliases.get(descriptor.command) {
                return Err(NativeFacadeValidationError::AliasMatchesPrimary {
                    alias: descriptor.command,
                    alias_command,
                    primary_command: descriptor.command,
                });
            }

            for &alias in descriptor.aliases {
                validate_alias(descriptor.command, alias)?;
                if RESERVED_COMMANDS.contains(&alias) {
                    return Err(NativeFacadeValidationError::ReservedAlias {
                        command: descriptor.command,
                        alias,
                    });
                }
                if primaries.contains_key(alias) {
                    return Err(NativeFacadeValidationError::AliasMatchesPrimary {
                        alias,
                        alias_command: descriptor.command,
                        primary_command: alias,
                    });
                }
                if let Some(first_command) = aliases.insert(alias, descriptor.command) {
                    return Err(NativeFacadeValidationError::DuplicateAlias {
                        alias,
                        first_command,
                        second_command: descriptor.command,
                    });
                }
            }

            if vendor_identities
                .insert(descriptor.identity.as_str(), ())
                .is_some()
            {
                return Err(NativeFacadeValidationError::DuplicateVendorIdentity {
                    identity: descriptor.identity.as_str(),
                });
            }

            entries.push(RegisteredNativeFacade {
                descriptor,
                native_agent_type,
            });
        }

        Ok(Self { entries })
    }

    /// Resolves a primary command or alias without choosing a lane.
    pub fn resolve(&self, spelling: &str) -> Option<ResolvedNativeFacade<'_>> {
        self.entries
            .iter()
            .find(|entry| {
                entry.descriptor.command == spelling || entry.descriptor.aliases.contains(&spelling)
            })
            .map(|entry| ResolvedNativeFacade {
                descriptor: entry.descriptor,
                native_agent_type: entry.native_agent_type.as_ref(),
            })
    }

    /// Resolves a canonical primary command without choosing a lane.
    pub fn resolve_primary(&self, command: &str) -> Option<ResolvedNativeFacade<'_>> {
        self.entries
            .iter()
            .find(|entry| entry.descriptor.command == command)
            .map(|entry| ResolvedNativeFacade {
                descriptor: entry.descriptor,
                native_agent_type: entry.native_agent_type.as_ref(),
            })
    }

    /// Primary commands whose static native-lane policy is enabled.
    pub fn enabled_native_commands(&self) -> Vec<&'a str> {
        self.entries
            .iter()
            .filter(|entry| entry.descriptor.native.is_some_and(|lane| lane.enabled()))
            .map(|entry| entry.descriptor.command)
            .collect()
    }

    /// Primary commands whose static structured-lane policy is enabled.
    pub fn enabled_structured_commands(&self) -> Vec<&'a str> {
        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .descriptor
                    .structured
                    .is_some_and(|lane| lane.enabled())
            })
            .map(|entry| entry.descriptor.command)
            .collect()
    }
}

fn validate_vendor_identity(identity: VendorIdentity) -> Result<(), NativeFacadeValidationError> {
    token_error(identity.as_str()).map_err(|reason| {
        NativeFacadeValidationError::InvalidVendorIdentity {
            identity: identity.as_str(),
            reason,
        }
    })
}

fn validate_native_adapter(
    command: &'static str,
    adapter: NativeAdapterId,
) -> Result<(), NativeFacadeValidationError> {
    token_error(adapter.as_str()).map_err(|reason| {
        NativeFacadeValidationError::InvalidNativeAdapterId {
            command,
            adapter: adapter.as_str(),
            reason,
        }
    })
}

fn validate_structured_identity(
    command: &'static str,
    identity: StructuredAgentIdentity,
) -> Result<(), NativeFacadeValidationError> {
    token_error(identity.name()).map_err(|reason| {
        NativeFacadeValidationError::InvalidStructuredAgentIdentity {
            command,
            identity: identity.name(),
            reason,
        }
    })?;
    if identity.protocol_version() == 0 {
        return Err(NativeFacadeValidationError::InvalidStructuredProtocolVersion { command });
    }
    Ok(())
}

fn validate_structured_adapter(
    command: &'static str,
    adapter: StructuredAdapterId,
) -> Result<(), NativeFacadeValidationError> {
    token_error(adapter.as_str()).map_err(|reason| {
        NativeFacadeValidationError::InvalidStructuredAdapterId {
            command,
            adapter: adapter.as_str(),
            reason,
        }
    })
}

fn validate_command(command: &'static str) -> Result<(), NativeFacadeValidationError> {
    token_error(command)
        .map_err(|reason| NativeFacadeValidationError::InvalidCommand { command, reason })
}

fn validate_alias(
    command: &'static str,
    alias: &'static str,
) -> Result<(), NativeFacadeValidationError> {
    token_error(alias).map_err(|reason| NativeFacadeValidationError::InvalidAlias {
        command,
        alias,
        reason,
    })
}

fn validate_executable(
    command: &'static str,
    executable: &'static str,
) -> Result<(), NativeFacadeValidationError> {
    executable_error(executable).map_err(|reason| NativeFacadeValidationError::InvalidExecutable {
        command,
        executable,
        reason,
    })
}

fn token_error(token: &str) -> Result<(), NativeFacadeTokenError> {
    if token.is_empty() {
        Err(NativeFacadeTokenError::Empty)
    } else if !token.is_ascii() {
        Err(NativeFacadeTokenError::NonAscii)
    } else if token.starts_with('-') {
        Err(NativeFacadeTokenError::LeadingDash)
    } else {
        Ok(())
    }
}

fn executable_error(executable: &str) -> Result<(), NativeFacadeExecutableError> {
    if executable.is_empty() {
        Err(NativeFacadeExecutableError::Empty)
    } else if !executable.is_ascii() {
        Err(NativeFacadeExecutableError::NonAscii)
    } else if executable.starts_with('-') {
        Err(NativeFacadeExecutableError::LeadingDash)
    } else if executable.contains('/') || executable.contains('\\') {
        Err(NativeFacadeExecutableError::PathSeparator)
    } else {
        Ok(())
    }
}

/// One native facade per terminal harness marion names — `marion <harness> <its own flags>`.
///
/// This is the registry's shape, not a menu: the sweep
/// `production_facades_cover_every_terminal_harness_exactly_once` holds it to
/// [`crate::harness::Harness`] minus `Acp` (a protocol with no binary of its own to stand in
/// front of), so adding a harness there without a row here is a failing test, and a facade here
/// that names no harness is too. Each native lane's adapter id is the harness's wire spelling,
/// because the adapter **is** the harness's row (`marion_harness::native_adapter`), not a
/// separate implementation.
///
/// `enabled` is where a lane has been measured on the **interactive** shape the facade runs,
/// and the reason is stated where it is not:
///
/// * `claude`, `codex` — the row's TUI shape was measured with its declaration flag
///   (`--mcp-config`, `-c mcp_servers.marion.*`) for M3's pane work, and again through the shipped
///   facade by `tests/native_facade_e2e.rs` (2026-09-05).
/// * `gemini` — enabled 2026-09-10, on measurement of the question that kept it dark: the
///   system-settings layer the declaration rides **outranks the operator's own**, and a native
///   node that silently lost the operator's servers would be §6.4's failure. gemini 0.53.0 merges
///   `mcpServers` **per key** (`tests/fixtures/s30/`, darwin 25.5.0): with the operator's
///   `~/.gemini/settings.json` declaring `pencil` and `GEMINI_CLI_SYSTEM_SETTINGS_PATH` declaring
///   `marion` the way the row emits it, the TUI's `/mcp` lists both (`🟢 pencil - Ready (1
///   tool)`, `🟢 marion - Ready (1 tool)`, status bar `2 MCP servers`), `gemini mcp list` connects
///   both, and the operator's `general.vimMode` survives the row's `general` block (`[INSERT]` on
///   screen). The bundle agrees: `SETTINGS_SCHEMA.mcpServers.mergeStrategy = "shallow_merge"`
///   (`{...user, ...system}`), applied in `mergeSettings` after user and workspace. Same-key
///   collision: the system layer's entry wins, so an operator-owned server named `marion` is
///   shadowed for the node's life (`s30/mcp-list.collision.txt`).
/// * `opencode`, `copilot` — enabled 2026-09-05, on measurement of the TUI's own reading of the
///   declaration through the shipped facade (`tests/native_facade_e2e.rs` fixture, darwin
///   25.5.0). opencode 1.17.3 with `OPENCODE_CONFIG_CONTENT`: its `/mcp` dialog lists
///   `marion connected ✓ Enabled` **beside the operator's own servers** (`pencil`, `semble`), so
///   the inline document merges rather than replaces; the status bar counts marion in `⊙ 2 MCP`.
///   copilot 1.0.83 with `--additional-mcp-config @<path>`: its `/mcp` view lists
///   `marion · User · mcp:marion · 646 tokens` beside the built-in `github-mcp-server`, `2/2
///   enabled` — a token count is a `tools/list` the bridge answered. Both lanes pass the full
///   matrix (first screen through `marion_term`, opaque keystroke, resize, detach, kill).
///
/// No structured lanes: `marion run <agent-type>` is that surface, and it is not this registry's.
pub const PRODUCTION_NATIVE_FACADES: &[NativeFacadeDescriptor] = &[
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("claude"),
        command: "claude",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("claude", "claude", NativeAdapterId::new("claude-code")),
        )),
        structured: None,
    },
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("codex"),
        command: "codex",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("codex", "codex", NativeAdapterId::new("codex")),
        )),
        structured: None,
    },
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("gemini"),
        command: "gemini",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("gemini", "gemini", NativeAdapterId::new("gemini")),
        )),
        structured: None,
    },
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("opencode"),
        command: "opencode",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("opencode", "opencode", NativeAdapterId::new("opencode")),
        )),
        structured: None,
    },
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("copilot"),
        command: "copilot",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("copilot", "copilot", NativeAdapterId::new("copilot")),
        )),
        structured: None,
    },
    // Disabled 2026-09-05: the `--with-extension` declaration channel was measured on the headless
    // `run -t` surface (S26) and its reading by goose's interactive TUI has not been — the same
    // reason opencode's and copilot's lanes ship off.
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("goose"),
        command: "goose",
        aliases: &[],
        native: Some(Lane::new(
            false,
            NativeLane::new("goose", "goose", NativeAdapterId::new("goose")),
        )),
        structured: None,
    },
    // Disabled 2026-09-05: the `CLINE_MCP_SETTINGS_PATH` declaration channel was measured on the
    // headless `--json` surface (S27) and its reading by cline's interactive TUI has not been.
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("cline"),
        command: "cline",
        aliases: &[],
        native: Some(Lane::new(
            false,
            NativeLane::new("cline", "cline", NativeAdapterId::new("cline")),
        )),
        structured: None,
    },
    // Disabled 2026-09-05: the `--mcp-config` declaration channel was measured on the headless
    // `-p` surface (S25) and its reading by qwen's interactive TUI has not been.
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("qwen"),
        command: "qwen",
        aliases: &[],
        native: Some(Lane::new(
            false,
            NativeLane::new("qwen", "qwen", NativeAdapterId::new("qwen")),
        )),
        structured: None,
    },
];

/// The production registry: [`PRODUCTION_NATIVE_FACADES`], validated.
pub fn production_native_facades() -> NativeFacadeRegistry<'static> {
    NativeFacadeRegistry::new(PRODUCTION_NATIVE_FACADES)
        .expect("the built-in native facade descriptor slice is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn native(
        enabled: bool,
        executable: &'static str,
        agent_type: &'static str,
    ) -> NativeFacadeNativeLane {
        Lane::new(
            enabled,
            NativeLane::new(
                executable,
                agent_type,
                NativeAdapterId::new("synthetic-native"),
            ),
        )
    }

    const fn structured(
        enabled: bool,
        identity: &'static str,
        control: StructuredControl,
    ) -> NativeFacadeStructuredLane {
        Lane::new(
            enabled,
            StructuredLane::new(
                StructuredAgentIdentity::new(identity, 1),
                control,
                StructuredAdapterId::new("synthetic-structured"),
            ),
        )
    }

    const ATLAS: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &["at"],
        native: Some(native(true, "atlas-cli", "codex")),
        structured: Some(structured(false, "atlas-acp", StructuredControl::Acp)),
    };
    const BOREAL: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("boreal"),
        command: "boreal",
        aliases: &["bo"],
        native: Some(native(true, "boreal-cli", "codex")),
        structured: None,
    };

    #[test]
    fn resolves_an_exact_primary_name() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        assert_eq!(
            registry
                .resolve("atlas")
                .map(|resolved| resolved.descriptor()),
            Some(&ATLAS)
        );
    }

    #[test]
    fn resolves_an_explicit_alias() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        assert_eq!(
            registry.resolve("at").map(|resolved| resolved.descriptor()),
            Some(&ATLAS)
        );
    }

    #[test]
    fn client_alias_resolution_and_handler_primary_binding_are_distinct() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        assert_eq!(
            registry.resolve("at").map(|resolved| resolved.descriptor()),
            Some(&ATLAS)
        );
        assert_eq!(
            registry
                .resolve_primary("atlas")
                .map(|resolved| resolved.descriptor()),
            Some(&ATLAS)
        );
        assert!(registry.resolve_primary("at").is_none());
    }

    #[test]
    fn rejects_invalid_executable_names() {
        for (executable, reason) in [
            ("", NativeFacadeExecutableError::Empty),
            ("atlás", NativeFacadeExecutableError::NonAscii),
            ("-atlas", NativeFacadeExecutableError::LeadingDash),
            ("../atlas", NativeFacadeExecutableError::PathSeparator),
            ("atlas\\\\cli", NativeFacadeExecutableError::PathSeparator),
        ] {
            let descriptor = NativeFacadeDescriptor {
                native: Some(native(true, executable, "codex")),
                ..ATLAS
            };

            assert_eq!(
                NativeFacadeRegistry::new(&[descriptor]).unwrap_err(),
                NativeFacadeValidationError::InvalidExecutable {
                    command: "atlas",
                    executable,
                    reason,
                }
            );
        }
    }

    #[test]
    fn structured_control_metadata_distinguishes_acp_from_mcp() {
        let mcp = NativeFacadeDescriptor {
            structured: Some(structured(true, "atlas-mcp", StructuredControl::Mcp)),
            ..ATLAS
        };

        assert_ne!(
            mcp.structured.unwrap().control(),
            ATLAS.structured.unwrap().transport()
        );
    }

    #[test]
    fn lookup_is_case_sensitive() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        assert!(registry.resolve("Atlas").is_none());
        assert!(registry.resolve("AT").is_none());
    }

    #[test]
    fn unknown_input_is_not_discovered_from_path() {
        let registry = NativeFacadeRegistry::new(&[ATLAS]).unwrap();

        // `sh` is an executable on the test host's PATH, but a pure registry only resolves names
        // explicitly registered in its descriptors.
        assert!(registry.resolve("sh").is_none());
    }

    #[test]
    fn rejects_a_reserved_primary_name() {
        let descriptor = NativeFacadeDescriptor {
            command: "help",
            ..ATLAS
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[descriptor]).unwrap_err(),
            NativeFacadeValidationError::ReservedCommand { command: "help" }
        );
    }

    #[test]
    fn rejects_a_reserved_alias() {
        let descriptor = NativeFacadeDescriptor {
            aliases: &["doctor"],
            ..ATLAS
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[descriptor]).unwrap_err(),
            NativeFacadeValidationError::ReservedAlias {
                command: "atlas",
                alias: "doctor",
            }
        );
    }

    #[test]
    fn rejects_each_omitted_control_verb_as_a_primary_and_alias() {
        for (verb, aliases) in [
            ("run", &["run"] as &[_]),
            ("attach", &["attach"]),
            ("tree", &["tree"]),
            ("mcp", &["mcp"]),
        ] {
            let primary = NativeFacadeDescriptor {
                command: verb,
                ..ATLAS
            };
            assert_eq!(
                NativeFacadeRegistry::new(&[primary]).unwrap_err(),
                NativeFacadeValidationError::ReservedCommand { command: verb }
            );

            let alias = NativeFacadeDescriptor { aliases, ..ATLAS };
            assert_eq!(
                NativeFacadeRegistry::new(&[alias]).unwrap_err(),
                NativeFacadeValidationError::ReservedAlias {
                    command: "atlas",
                    alias: verb,
                }
            );
        }
    }

    #[test]
    fn rejects_duplicate_primary_names() {
        let duplicate = NativeFacadeDescriptor {
            aliases: &["atlas-two"],
            ..ATLAS
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[ATLAS, duplicate]).unwrap_err(),
            NativeFacadeValidationError::DuplicatePrimary { command: "atlas" }
        );
    }

    #[test]
    fn a_primary_alias_collision_names_both_owners_in_either_registration_order() {
        let alias_owner = NativeFacadeDescriptor {
            aliases: &["boreal"],
            ..ATLAS
        };

        for descriptors in [[alias_owner, BOREAL], [BOREAL, alias_owner]] {
            assert_eq!(
                NativeFacadeRegistry::new(&descriptors).unwrap_err(),
                NativeFacadeValidationError::AliasMatchesPrimary {
                    alias: "boreal",
                    alias_command: "atlas",
                    primary_command: "boreal",
                }
            );
        }
    }

    #[test]
    fn rejects_an_alias_that_collides_with_another_alias() {
        let duplicate_alias = NativeFacadeDescriptor {
            identity: VendorIdentity::new("cinder"),
            command: "cinder",
            aliases: &["at"],
            native: Some(native(true, "cinder-cli", "codex")),
            structured: None,
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[ATLAS, duplicate_alias]).unwrap_err(),
            NativeFacadeValidationError::DuplicateAlias {
                alias: "at",
                first_command: "atlas",
                second_command: "cinder",
            }
        );
    }

    #[test]
    fn rejects_invalid_command_and_alias_tokens() {
        for (descriptor, error) in [
            (
                NativeFacadeDescriptor {
                    command: "",
                    ..ATLAS
                },
                NativeFacadeValidationError::InvalidCommand {
                    command: "",
                    reason: NativeFacadeTokenError::Empty,
                },
            ),
            (
                NativeFacadeDescriptor {
                    command: "météore",
                    ..ATLAS
                },
                NativeFacadeValidationError::InvalidCommand {
                    command: "météore",
                    reason: NativeFacadeTokenError::NonAscii,
                },
            ),
            (
                NativeFacadeDescriptor {
                    command: "-atlas",
                    ..ATLAS
                },
                NativeFacadeValidationError::InvalidCommand {
                    command: "-atlas",
                    reason: NativeFacadeTokenError::LeadingDash,
                },
            ),
            (
                NativeFacadeDescriptor {
                    aliases: &["-at"],
                    ..ATLAS
                },
                NativeFacadeValidationError::InvalidAlias {
                    command: "atlas",
                    alias: "-at",
                    reason: NativeFacadeTokenError::LeadingDash,
                },
            ),
        ] {
            assert_eq!(NativeFacadeRegistry::new(&[descriptor]).unwrap_err(), error);
        }
    }

    #[test]
    fn current_computed_readiness_never_becomes_descriptor_state() {
        let registry = NativeFacadeRegistry::new(&[ATLAS, BOREAL]).unwrap();

        assert_eq!(
            registry
                .resolve("boreal")
                .map(|resolved| resolved.descriptor()),
            Some(&BOREAL)
        );
        assert!(
            !registry
                .resolve("boreal")
                .unwrap()
                .native_lane()
                .unwrap()
                .is_bindable(LaneReadiness::evaluate(true, true, true, true, false))
        );
        assert!(
            registry
                .resolve("atlas")
                .unwrap()
                .native_lane()
                .unwrap()
                .is_bindable(LaneReadiness::evaluate(true, true, true, true, true))
        );
        assert_eq!(registry.enabled_native_commands(), vec!["atlas", "boreal"]);
    }

    /// **The registry's cardinality is the `Harness` enum's**, minus the one protocol row.
    ///
    /// Every terminal harness has exactly one facade whose native lane resolves to an agent type
    /// on that harness, whose adapter id is that harness's own spelling (the adapter is the row),
    /// and whose command is the executable it stands in front of; no facade names a harness
    /// twice, none names ACP, and no structured lane is advertised here. Reserved words stay
    /// unresolvable, and so does an unknown name.
    ///
    /// Mutation: drop a harness's descriptor, add a second `claude`, give a lane an ACP agent
    /// type, or advertise a structured lane.
    #[test]
    fn production_facades_cover_every_terminal_harness_exactly_once() {
        use crate::harness::Harness;

        let registry = production_native_facades();
        let mut covered: Vec<Harness> = Vec::new();
        for descriptor in PRODUCTION_NATIVE_FACADES {
            let resolved = registry
                .resolve_primary(descriptor.command)
                .unwrap_or_else(|| panic!("{} does not resolve", descriptor.command));
            let lane = resolved
                .native_lane()
                .unwrap_or_else(|| panic!("{} has no native lane", descriptor.command));
            let harness = lane.agent_type().harness;
            assert_ne!(
                harness,
                Harness::Acp,
                "{}: a protocol is not a facade",
                descriptor.command
            );
            assert_eq!(
                lane.lane().adapter().as_str(),
                harness.as_str(),
                "{}: the adapter id is the harness's row",
                descriptor.command
            );
            assert_eq!(
                lane.executable(),
                descriptor.command,
                "{}: the facade command is the executable it stands in front of",
                descriptor.command
            );
            assert!(
                descriptor.structured.is_none(),
                "{}: `marion run` is the structured surface",
                descriptor.command
            );
            assert!(
                !covered.contains(&harness),
                "{}: {harness} is advertised twice",
                descriptor.command
            );
            covered.push(harness);
        }
        let mut expected: Vec<Harness> = Harness::ALL
            .into_iter()
            .filter(|h| *h != Harness::Acp)
            .collect();
        expected.sort();
        covered.sort();
        assert_eq!(
            covered, expected,
            "every terminal harness has exactly one facade"
        );
        assert_eq!(
            registry.enabled_native_commands(),
            vec!["claude", "codex", "gemini", "opencode", "copilot"],
            "the enabled lanes are exactly those whose interactive shape was measured \
             (`tests/native_facade_e2e.rs`); `gemini`'s settings merge was measured per key \
             (`tests/fixtures/s30/`), so it is among them"
        );
        assert!(registry.enabled_structured_commands().is_empty());
        for word in RESERVED_COMMANDS
            .iter()
            .copied()
            .chain(["codex-native", "definitely-not-a-facade"])
        {
            assert!(registry.resolve(word).is_none(), "{word:?} resolves");
        }
    }

    #[test]
    fn registry_construction_rejects_an_unknown_native_agent_type() {
        let descriptor = NativeFacadeDescriptor {
            identity: VendorIdentity::new("unknown-agent"),
            command: "unknown-agent",
            aliases: &[],
            native: Some(native(false, "unknown-cli", "not-a-built-in-agent-type")),
            structured: None,
        };

        assert_eq!(
            NativeFacadeRegistry::new(&[descriptor]).unwrap_err(),
            NativeFacadeValidationError::UnknownNativeAgentType {
                command: "unknown-agent",
                agent_type: "not-a-built-in-agent-type",
            }
        );
    }

    #[test]
    fn registry_stores_the_canonical_native_agent_type() {
        let descriptor = NativeFacadeDescriptor {
            identity: VendorIdentity::new("codex"),
            command: "codex-native",
            aliases: &[],
            native: Some(native(true, "codex", "codex")),
            structured: None,
        };
        let descriptors = [descriptor];
        let registry = NativeFacadeRegistry::new(&descriptors).unwrap();

        assert_eq!(
            registry
                .resolve("codex-native")
                .unwrap()
                .native_lane()
                .unwrap()
                .agent_type()
                .name,
            "codex-impl"
        );
    }

    #[test]
    fn static_lane_policy_and_computed_readiness_are_distinct() {
        const LANE: Lane<NativeLane> = Lane::new(
            true,
            NativeLane::new("atlas-cli", "codex", NativeAdapterId::new("atlas-native")),
        );

        let ready = LaneReadiness::evaluate(true, true, true, true, true);
        let blocked = LaneReadiness::evaluate(true, true, true, true, false);

        assert!(LANE.enabled());
        assert!(LANE.is_bindable(ready));
        assert!(!LANE.is_bindable(blocked));
        assert_eq!(
            blocked.result(),
            LaneReadinessResult::Blocked(LaneReadinessBlock::BehaviorUnavailable)
        );
        assert!(blocked.enabled());
        assert!(blocked.installed());
        assert!(blocked.version_compatible());
        assert!(blocked.injection_ready());
        assert!(!blocked.behavior_ready());
    }

    #[test]
    fn descriptor_carries_stable_vendor_and_concrete_adapter_identities() {
        const DESCRIPTOR: NativeFacadeDescriptor = NativeFacadeDescriptor {
            identity: VendorIdentity::new("atlas"),
            command: "atlas",
            aliases: &["at"],
            native: Some(Lane::new(
                true,
                NativeLane::new("atlas-cli", "codex", NativeAdapterId::new("atlas-native")),
            )),
            structured: Some(Lane::new(
                true,
                StructuredLane::new(
                    StructuredAgentIdentity::new("atlas-acp", 1),
                    StructuredControl::Acp,
                    StructuredAdapterId::new("atlas-structured"),
                ),
            )),
        };

        assert_eq!(DESCRIPTOR.identity.as_str(), "atlas");
        assert_eq!(
            DESCRIPTOR.native.unwrap().config().adapter().as_str(),
            "atlas-native"
        );
        let structured_lane = DESCRIPTOR.structured.unwrap();
        let structured = structured_lane.config();
        assert_eq!(structured.agent_identity().name(), "atlas-acp");
        assert_eq!(structured.agent_identity().protocol_version(), 1);
        assert_eq!(structured.control(), StructuredControl::Acp);
        assert_eq!(structured.adapter().as_str(), "atlas-structured");
    }
}
