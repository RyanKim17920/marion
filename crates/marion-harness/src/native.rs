use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::mcp_bridge::MarionMcpBridge;

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

#[derive(Debug)]
pub struct NativeNodeContext<'a> {
    pub bridge: &'a MarionMcpBridge,
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
