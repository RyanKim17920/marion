//! The native boundary: how marion stands in front of the operator's own harness.
//!
//! A native node is the harness the operator already runs — their login, their settings, their
//! flags — with exactly one thing added: marion's MCP server. Everything here exists to make that
//! "exactly one thing" structural. [`NativeInjectionAdapter`] sees only a [`NativeNodeContext`] and
//! returns a [`NativeInjection`]; [`assemble_native`] alone sees the program and the operator's
//! argv, and places `program + prefix + tail`. There is **one** adapter, [`SpecNativeAdapter`],
//! and it is a reading of a [`HarnessSpec`] row's [`LiveDeclaration`]: a sixth harness gets a
//! native lane by writing the row, not an impl.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use marion_core::harness::Harness;

use crate::adapter::harness_spec;
use crate::mcp_bridge::BridgeEnv;
use crate::spec::{HarnessSpec, LiveDeclaration};

#[derive(Debug)]
pub struct NativeProcessBase {
    pub program: OsString,
    pub user_argv: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub geometry: NativeTerminalGeometry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeTerminalGeometry {
    pub cols: u16,
    pub rows: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct NativeEnvironmentView<'a> {
    entries: &'a [(OsString, OsString)],
}

impl<'a> NativeEnvironmentView<'a> {
    pub fn validate(entries: &'a [(OsString, OsString)]) -> Result<Self, NativeInjectionError> {
        validate_environment(entries, DuplicateKind::Base)?;
        Ok(Self { entries })
    }

    pub fn get(&self, name: &OsStr) -> Option<&'a OsStr> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_os_str())
    }
}

/// Everything a native adapter is allowed to know about the node it is preparing.
///
/// Deliberately **not** the program, the operator's argv, or the geometry: those are the
/// assembler's, and an adapter that could read the tail could also rewrite it.
#[derive(Debug)]
pub struct NativeNodeContext<'a> {
    /// marion's own MCP server, as every harness's declaration of it is written
    /// ([`BridgeEnv::pairs`]).
    pub bridge: &'a BridgeEnv,
    /// The node's own directory: the only place a document may be written.
    pub document_dir: &'a Path,
    pub allowed_marion_tools: &'a [&'a str],
    pub environment: NativeEnvironmentView<'a>,
}

#[derive(Debug)]
pub struct NativeInjection {
    pub argv_prefix: Vec<OsString>,
    pub env_overlay: Vec<(OsString, OsString)>,
    pub documents: Vec<NativeDocument>,
}

#[derive(Debug)]
pub struct NativeDocument {
    pub path: PathBuf,
    pub contents: Vec<u8>,
}

#[derive(Debug)]
pub struct NativeInvocation {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub geometry: NativeTerminalGeometry,
    environment_is_authoritative: EnvironmentIsAuthoritative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnvironmentIsAuthoritative;

impl NativeInvocation {
    pub const fn requires_env_clear(&self) -> bool {
        let _ = self.environment_is_authoritative;
        true
    }
}

#[derive(Debug)]
pub struct PreparedNativeLaunch {
    pub invocation: NativeInvocation,
    pub documents: Vec<NativeDocument>,
}

pub trait NativeInjectionAdapter: Send + Sync {
    fn prepare_native(
        &self,
        context: &NativeNodeContext<'_>,
    ) -> Result<NativeInjection, NativeInjectionError>;
}

/// **The** native adapter: a row's [`LiveDeclaration`], rendered.
///
/// One type for every harness, because the injection a native node needs is a fact the row
/// already states — which flag or variable carries marion's declaration, and what it says — and
/// stating it a second time in an impl is how a lane drifts from the launch it is supposed to
/// mirror. One more field of the row is read, [`HarnessSpec::updates`]: the measured switch that
/// keeps the harness from updating itself, because a binary that replaces itself or offers to
/// mid-session interrupts exactly the operator this lane fronts. Nothing else is: not the row's
/// argv (every other flag is the operator's), not its env (relocating `HOME` is isolation, and a
/// native node is not isolated). The sweep
/// `every_native_row_injects_only_marions_mcp_server_and_no_managed_flags` holds the declaration
/// to what the managed live launch of the same row writes, and
/// `every_row_states_its_update_policy_and_renders_it_into_every_launch_shape` holds the switch.
#[derive(Debug)]
pub struct SpecNativeAdapter {
    row: &'static HarnessSpec,
}

impl SpecNativeAdapter {
    /// The adapter for a row, or `None` where the row has no launch-time declaration channel.
    pub const fn for_row(row: &'static HarnessSpec) -> Option<Self> {
        match row.live_declaration {
            Some(_) => Some(Self { row }),
            None => None,
        }
    }

    pub const fn harness(&self) -> Harness {
        self.row.harness
    }
}

impl NativeInjectionAdapter for SpecNativeAdapter {
    fn prepare_native(
        &self,
        context: &NativeNodeContext<'_>,
    ) -> Result<NativeInjection, NativeInjectionError> {
        let declaration = self.row.live_declaration.ok_or_else(|| {
            NativeInjectionError::Adapter(format!(
                "harness {} has no launch-time MCP declaration channel",
                self.row.harness
            ))
        })?;
        let mut injection = NativeInjection {
            argv_prefix: Vec::new(),
            env_overlay: Vec::new(),
            documents: Vec::new(),
        };
        match declaration {
            LiveDeclaration::ArgvDocument {
                flag,
                file,
                prefix,
                body,
            } => {
                let path = context.document_dir.join(file);
                let mut named = OsString::from(prefix);
                named.push(path.as_os_str());
                injection.argv_prefix = vec![OsString::from(flag), named];
                injection.documents.push(NativeDocument {
                    path,
                    contents: body(context.bridge).into_bytes(),
                });
            }
            LiveDeclaration::EnvDocument { key, file, body } => {
                let path = context.document_dir.join(file);
                injection
                    .env_overlay
                    .push((OsString::from(key), path.as_os_str().to_owned()));
                injection.documents.push(NativeDocument {
                    path,
                    contents: body(context.bridge).into_bytes(),
                });
            }
            LiveDeclaration::EnvInline { key, body } => {
                injection
                    .env_overlay
                    .push((OsString::from(key), OsString::from(body(context.bridge))));
            }
            LiveDeclaration::ArgvPairs { flag, pairs, .. } => {
                // The update policy's pair first, exactly where the managed launch renders it
                // ([`crate::spec::render`]'s `Field::Pairs`), then the declaration's own.
                for (k, v) in self
                    .row
                    .updates
                    .pair()
                    .into_iter()
                    .chain(pairs(context.bridge))
                {
                    injection.argv_prefix.push(OsString::from(flag));
                    injection
                        .argv_prefix
                        .push(OsString::from(format!("{k}={v}")));
                }
            }
            LiveDeclaration::ArgvInline { flag, body, .. } => {
                injection.argv_prefix =
                    vec![OsString::from(flag), OsString::from(body(context.bridge))];
            }
        }
        // The row's completion push is enabled here as on the pane shape, and for the same reason
        // it leads: the flag is variadic, and the declaration flag behind it is what closes it
        // before the operator's own tail.
        injection
            .argv_prefix
            .splice(0..0, self.row.push.argv().iter().map(OsString::from));
        // The row's no-self-update switch is the one thing beside marion's declaration a native
        // node carries: a facade session is exactly where an update prompt interrupts the operator.
        if let Some((k, v)) = self.row.updates.env() {
            injection
                .env_overlay
                .push((OsString::from(k), OsString::from(v)));
        }
        Ok(injection)
    }
}

/// The native adapter for a harness — the registry's answer to "how does marion stand in front
/// of this one?" — or `None` where the harness has no launch-time declaration channel (ACP) and a
/// native launch on it must be refused by name.
///
/// A table of rows, not of impls: each entry is [`SpecNativeAdapter::for_row`] over
/// [`harness_spec`], and a harness marion names is refused here only because its row says so.
pub fn native_adapter(harness: Harness) -> Option<&'static dyn NativeInjectionAdapter> {
    static ADAPTERS: std::sync::OnceLock<Vec<Option<SpecNativeAdapter>>> =
        std::sync::OnceLock::new();
    let adapters = ADAPTERS.get_or_init(|| {
        Harness::ALL
            .iter()
            .map(|&h| SpecNativeAdapter::for_row(harness_spec(h)))
            .collect()
    });
    let index = Harness::ALL.iter().position(|&h| h == harness)?;
    adapters[index]
        .as_ref()
        .map(|adapter| adapter as &'static dyn NativeInjectionAdapter)
}

#[derive(Debug, thiserror::Error)]
pub enum NativeInjectionError {
    #[error("native environment name is empty")]
    EmptyEnvironmentName,
    #[error("native environment name contains '='")]
    InvalidEnvironmentName,
    #[error("native environment contains a duplicate name")]
    DuplicateEnvironmentName,
    #[error("native process value contains an embedded NUL")]
    EmbeddedNul,
    #[error("native injection overlay contains a duplicate name")]
    DuplicateOverlayName,
    #[error("native process environment may not inject reserved MARION_ identity")]
    ReservedMarionEnvironment,
    #[error("native adapter injection failed: {0}")]
    Adapter(String),
}

#[derive(Clone, Copy)]
enum DuplicateKind {
    Base,
    Overlay,
}

fn contains_nul(value: &OsStr) -> bool {
    value.as_encoded_bytes().contains(&0)
}

fn validate_environment(
    entries: &[(OsString, OsString)],
    duplicate_kind: DuplicateKind,
) -> Result<(), NativeInjectionError> {
    let mut names = HashSet::with_capacity(entries.len());
    for (name, value) in entries {
        if contains_nul(name) || contains_nul(value) {
            return Err(NativeInjectionError::EmbeddedNul);
        }
        let bytes = name.as_encoded_bytes();
        if bytes.is_empty() {
            return Err(NativeInjectionError::EmptyEnvironmentName);
        }
        if bytes.contains(&b'=') {
            return Err(NativeInjectionError::InvalidEnvironmentName);
        }
        if !names.insert(name.as_os_str()) {
            return Err(match duplicate_kind {
                DuplicateKind::Base => NativeInjectionError::DuplicateEnvironmentName,
                DuplicateKind::Overlay => NativeInjectionError::DuplicateOverlayName,
            });
        }
    }
    Ok(())
}

fn is_reserved_marion_name(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b"MARION_")
}

pub fn validate_native_process_values(
    program: &OsStr,
    argv: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Path,
) -> Result<(), NativeInjectionError> {
    if contains_nul(program)
        || argv.iter().any(|argument| contains_nul(argument))
        || contains_nul(cwd.as_os_str())
    {
        return Err(NativeInjectionError::EmbeddedNul);
    }
    validate_environment(env, DuplicateKind::Base)
}

pub fn assemble_native(
    base: NativeProcessBase,
    injection: NativeInjection,
) -> Result<PreparedNativeLaunch, NativeInjectionError> {
    validate_native_process_values(&base.program, &base.user_argv, &base.env, &base.cwd)?;
    if injection
        .argv_prefix
        .iter()
        .any(|argument| contains_nul(argument))
    {
        return Err(NativeInjectionError::EmbeddedNul);
    }
    validate_environment(&injection.env_overlay, DuplicateKind::Overlay)?;
    if injection
        .env_overlay
        .iter()
        .any(|(name, _)| is_reserved_marion_name(name))
    {
        return Err(NativeInjectionError::ReservedMarionEnvironment);
    }

    let NativeProcessBase {
        program,
        user_argv,
        env,
        cwd,
        geometry,
    } = base;
    let NativeInjection {
        mut argv_prefix,
        env_overlay,
        documents,
    } = injection;

    argv_prefix.extend(user_argv);

    let mut env = env
        .into_iter()
        .filter(|(name, _)| !is_reserved_marion_name(name))
        .collect::<Vec<_>>();
    for (name, value) in env_overlay {
        if let Some(position) = env.iter().position(|(candidate, _)| candidate == &name) {
            env[position].1 = value;
        } else {
            env.push((name, value));
        }
    }

    Ok(PreparedNativeLaunch {
        invocation: NativeInvocation {
            program,
            args: argv_prefix,
            env,
            cwd,
            geometry,
            environment_is_authoritative: EnvironmentIsAuthoritative,
        },
        documents,
    })
}
