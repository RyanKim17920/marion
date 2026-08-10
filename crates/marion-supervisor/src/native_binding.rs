use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use marion_core::{NativeFacadeDescriptor, NativeFacadeRegistry, NativeFacadeTransport};
use marion_proto::{
    NativeLaunchContext, NativeLaunchContextV2, NativeOsValueConversionError, TerminalGeometryV1,
};

/// A validated V2 launch bound to one ready facade and its exact declared executable.
#[derive(Debug, PartialEq, Eq)]
pub struct BoundNativeLaunch<'a> {
    pub descriptor: &'a NativeFacadeDescriptor,
    pub program: OsString,
    pub argv: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub geometry: TerminalGeometryV1,
}

/// Why byte-exact native launch state could not be bound to a declared facade.
#[derive(Debug, thiserror::Error)]
pub enum NativeBindingError {
    #[error("native launch V1 carries no facade selector and cannot be bound")]
    SelectorMissingInV1,
    #[error("native facade command {0:?} is not a ready canonical primary")]
    NonCanonicalFacadeCommand(String),
    #[error("native facade command {0:?} is unavailable")]
    FacadeUnavailable(String),
    #[error("facade {facade:?} is bound to a different agent type than {requested:?}")]
    AgentTypeMismatch { facade: String, requested: String },
    #[error("native injection requires TransparentPty, not {0:?}")]
    TransportMismatch(NativeFacadeTransport),
    #[error("native launch environment has no PATH")]
    MissingPath,
    #[error("native launch contains an invalid environment name")]
    InvalidEnvironmentName,
    #[error("native launch contains a duplicate environment name")]
    DuplicateEnvironmentName,
    #[error("native launch contains an embedded NUL")]
    EmbeddedNul,
    #[error("declared executable {0:?} was not found on the submitted PATH")]
    ProgramNotFound(String),
    #[error("submitted native program does not equal the declared executable resolution")]
    ProgramMismatch,
    #[error(transparent)]
    UnsupportedPlatform(#[from] NativeOsValueConversionError),
}

fn contains_nul(value: &OsStr) -> bool {
    value.as_encoded_bytes().contains(&0)
}

fn validate_environment(env: &[(OsString, OsString)]) -> Result<(), NativeBindingError> {
    if env
        .iter()
        .any(|(name, value)| contains_nul(name) || contains_nul(value))
    {
        return Err(NativeBindingError::EmbeddedNul);
    }

    let mut names = HashSet::with_capacity(env.len());
    for (name, _) in env {
        let bytes = name.as_encoded_bytes();
        if bytes.is_empty() || bytes.contains(&b'=') {
            return Err(NativeBindingError::InvalidEnvironmentName);
        }
        if !names.insert(name.clone()) {
            return Err(NativeBindingError::DuplicateEnvironmentName);
        }
    }
    Ok(())
}

pub(crate) fn validate_launch_values(
    program: &OsStr,
    argv: &[OsString],
    cwd: &Path,
    env: &[(OsString, OsString)],
) -> Result<(), NativeBindingError> {
    if contains_nul(program)
        || argv.iter().any(|argument| contains_nul(argument))
        || contains_nul(cwd.as_os_str())
    {
        return Err(NativeBindingError::EmbeddedNul);
    }
    validate_environment(env)
}

/// Resolve only the declared executable from the exact submitted `PATH` and cwd.
pub fn resolve_declared_executable(
    executable: &str,
    cwd: &Path,
    env: &[(OsString, OsString)],
) -> Result<OsString, NativeBindingError> {
    #[cfg(not(unix))]
    {
        let _ = (executable, cwd, env);
        return Err(NativeBindingError::UnsupportedPlatform(
            NativeOsValueConversionError::UnsupportedPlatform,
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if executable.as_bytes().contains(&0) || contains_nul(cwd.as_os_str()) {
            return Err(NativeBindingError::EmbeddedNul);
        }
        validate_environment(env)?;
        let path = env
            .iter()
            .find(|(name, _)| name == OsStr::new("PATH"))
            .map(|(_, value)| value)
            .ok_or(NativeBindingError::MissingPath)?;

        for component in std::env::split_paths(path) {
            let directory = if component.is_absolute() {
                component
            } else {
                cwd.join(component)
            };
            let candidate = directory.join(executable);
            let Ok(metadata) = std::fs::metadata(&candidate) else {
                continue;
            };
            if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                return Ok(candidate.into_os_string());
            }
        }

        Err(NativeBindingError::ProgramNotFound(executable.into()))
    }
}

struct ConvertedNativeLaunch {
    program: OsString,
    argv: Vec<OsString>,
    cwd: PathBuf,
    env: Vec<(OsString, OsString)>,
}

fn convert_v2(
    context: &NativeLaunchContextV2,
) -> Result<ConvertedNativeLaunch, NativeBindingError> {
    let program = context.program.to_os_string()?;
    let argv = context
        .argv
        .iter()
        .map(|argument| argument.to_os_string())
        .collect::<Result<Vec<_>, _>>()?;
    let cwd = PathBuf::from(context.cwd.to_os_string()?);
    let env = context
        .env
        .iter()
        .map(|entry| Ok((entry.name.to_os_string()?, entry.value.to_os_string()?)))
        .collect::<Result<Vec<_>, NativeOsValueConversionError>>()?;
    validate_launch_values(&program, &argv, &cwd, &env)?;
    Ok(ConvertedNativeLaunch {
        program,
        argv,
        cwd,
        env,
    })
}

/// Bind a V2 launch to one ready canonical facade without allocating launch resources.
pub fn bind_native_launch<'a>(
    context: &NativeLaunchContext,
    requested_agent_type: &str,
    registry: &'a NativeFacadeRegistry<'a>,
) -> Result<BoundNativeLaunch<'a>, NativeBindingError> {
    let NativeLaunchContext::V2(context) = context else {
        return Err(NativeBindingError::SelectorMissingInV1);
    };
    let ConvertedNativeLaunch {
        program,
        argv,
        cwd,
        env,
    } = convert_v2(context)?;

    let Some(descriptor) = registry.resolve_primary_for_launch(&context.facade_command) else {
        if registry
            .resolve_for_launch(&context.facade_command)
            .is_some()
        {
            return Err(NativeBindingError::NonCanonicalFacadeCommand(
                context.facade_command.clone(),
            ));
        }
        return Err(NativeBindingError::FacadeUnavailable(
            context.facade_command.clone(),
        ));
    };
    if descriptor.agent_type != requested_agent_type {
        return Err(NativeBindingError::AgentTypeMismatch {
            facade: descriptor.command.into(),
            requested: requested_agent_type.into(),
        });
    }
    if descriptor.transport != NativeFacadeTransport::TransparentPty {
        return Err(NativeBindingError::TransportMismatch(descriptor.transport));
    }

    let declared = resolve_declared_executable(descriptor.executable, &cwd, &env)?;
    if program != declared {
        return Err(NativeBindingError::ProgramMismatch);
    }

    Ok(BoundNativeLaunch {
        descriptor,
        program,
        argv,
        env,
        cwd,
        geometry: context.geometry.clone(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use marion_core::{
        NativeFacadeDescriptor, NativeFacadeReadiness, NativeFacadeRegistry, NativeFacadeTransport,
        production_native_facades,
    };
    use marion_proto::{
        NativeEnvVarV1, NativeLaunchContext, NativeLaunchContextV1, NativeLaunchContextV2,
        OpaqueOsValueV1, TerminalGeometryV1,
    };
    use marion_testsupport::{Scratch, scratch};

    use super::*;

    const READY: NativeFacadeDescriptor = NativeFacadeDescriptor {
        command: "atlas",
        aliases: &["at"],
        executable: "atlas-cli",
        agent_type: "atlas-agent",
        transport: NativeFacadeTransport::TransparentPty,
        readiness: NativeFacadeReadiness::Ready,
    };
    const PLANNED: NativeFacadeDescriptor = NativeFacadeDescriptor {
        command: "boreal",
        aliases: &["bo"],
        executable: "boreal-cli",
        agent_type: "boreal-agent",
        transport: NativeFacadeTransport::TransparentPty,
        readiness: NativeFacadeReadiness::Planned,
    };

    fn opaque(value: &OsStr) -> OpaqueOsValueV1 {
        OpaqueOsValueV1::from_os_str(value).expect("Unix preserves native launch bytes")
    }

    fn path_env(path: &Path) -> Vec<NativeEnvVarV1> {
        vec![NativeEnvVarV1 {
            name: opaque(OsStr::new("PATH")),
            value: opaque(path.as_os_str()),
        }]
    }

    fn env_var(name: &OsStr, value: &OsStr) -> NativeEnvVarV1 {
        NativeEnvVarV1 {
            name: opaque(name),
            value: opaque(value),
        }
    }

    fn geometry() -> TerminalGeometryV1 {
        TerminalGeometryV1 {
            cols: 101,
            rows: 37,
            xpixel: 3,
            ypixel: 5,
        }
    }

    fn executable_fixture(tag: &str) -> (Scratch, PathBuf, PathBuf) {
        let work = scratch(tag);
        let bin = work.join("bin");
        std::fs::create_dir(&bin).expect("the fixture bin directory exists");
        let executable = bin.join(READY.executable);
        std::fs::write(&executable, b"fixture executable\n")
            .expect("the fixture executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the fixture executable is executable");
        (work, bin, executable)
    }

    fn v2_context(
        command: &str,
        program: &OsStr,
        argv: &[&OsStr],
        cwd: &Path,
        env: Vec<NativeEnvVarV1>,
    ) -> NativeLaunchContext {
        NativeLaunchContext::V2(NativeLaunchContextV2::new(
            command.into(),
            opaque(program),
            argv.iter().map(|argument| opaque(argument)).collect(),
            opaque(cwd.as_os_str()),
            env,
            geometry(),
        ))
    }

    #[test]
    fn canonical_ready_facade_binds_to_its_exact_declared_program() {
        let (_work, bin, executable) = executable_fixture("native-binding-success");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let context = v2_context(
            "atlas",
            executable.as_os_str(),
            &[OsStr::new("--native")],
            &bin,
            path_env(&bin),
        );

        let bound = bind_native_launch(&context, "atlas-agent", &registry)
            .expect("the exact ready facade launch binds");

        assert_eq!(bound.descriptor, &READY);
        assert_eq!(bound.program, executable.as_os_str());
        assert_eq!(bound.argv, vec![OsString::from("--native")]);
        assert_eq!(bound.cwd, bin);
        assert_eq!(bound.env, vec![(OsString::from("PATH"), bin.into())]);
        assert_eq!(bound.geometry, geometry());
    }

    #[test]
    fn v1_is_refused_before_program_or_environment_lookup() {
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let context = NativeLaunchContext::V1(NativeLaunchContextV1::new(
            opaque(OsStr::from_bytes(b"missing\0program")),
            vec![],
            opaque(OsStr::new("/missing/cwd")),
            vec![],
            geometry(),
        ));

        assert!(matches!(
            bind_native_launch(&context, "atlas-agent", &registry),
            Err(NativeBindingError::SelectorMissingInV1)
        ));
    }

    #[test]
    fn an_alias_is_not_accepted_as_a_wire_selector() {
        let (work, bin, executable) = executable_fixture("native-binding-alias");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let alias_context = NativeLaunchContext::V2(NativeLaunchContextV2::new(
            "at".into(),
            opaque(executable.as_os_str()),
            vec![],
            opaque(work.as_os_str()),
            path_env(&bin),
            geometry(),
        ));

        assert!(matches!(
            bind_native_launch(&alias_context, "atlas-agent", &registry),
            Err(NativeBindingError::NonCanonicalFacadeCommand(command)) if command == "at"
        ));
    }

    #[test]
    fn unknown_planned_and_empty_production_commands_are_unavailable() {
        let (work, bin, executable) = executable_fixture("native-binding-unavailable");
        let descriptors = [READY, PLANNED];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");

        for command in ["unknown", "boreal", "bo"] {
            let context = v2_context(command, executable.as_os_str(), &[], &work, path_env(&bin));
            assert!(matches!(
                bind_native_launch(&context, "atlas-agent", &registry),
                Err(NativeBindingError::FacadeUnavailable(unavailable)) if unavailable == command
            ));
        }

        let context = v2_context("atlas", executable.as_os_str(), &[], &work, path_env(&bin));
        assert!(matches!(
            bind_native_launch(&context, "atlas-agent", &production_native_facades()),
            Err(NativeBindingError::FacadeUnavailable(command)) if command == "atlas"
        ));
    }

    #[test]
    fn requested_agent_type_must_match_the_descriptor() {
        let (work, bin, executable) = executable_fixture("native-binding-agent");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let context = v2_context("atlas", executable.as_os_str(), &[], &work, path_env(&bin));

        assert!(matches!(
            bind_native_launch(&context, "other-agent", &registry),
            Err(NativeBindingError::AgentTypeMismatch { facade, requested })
                if facade == "atlas" && requested == "other-agent"
        ));
    }

    #[test]
    fn typed_and_recursive_transports_do_not_enter_the_native_pty_kernel() {
        let (work, bin, executable) = executable_fixture("native-binding-transport");
        for transport in [
            NativeFacadeTransport::TypedAcp,
            NativeFacadeTransport::RecursiveMarion,
        ] {
            let descriptor = NativeFacadeDescriptor { transport, ..READY };
            let registry = NativeFacadeRegistry::new(std::slice::from_ref(&descriptor))
                .expect("the registry is valid");
            let context = v2_context("atlas", executable.as_os_str(), &[], &work, path_env(&bin));

            assert!(matches!(
                bind_native_launch(&context, "atlas-agent", &registry),
                Err(NativeBindingError::TransportMismatch(actual)) if actual == transport
            ));
        }
    }

    #[test]
    fn a_right_basename_at_an_arbitrary_path_does_not_bind() {
        let (work, bin, _executable) = executable_fixture("native-binding-program-mismatch");
        let arbitrary = work.join("elsewhere").join(READY.executable);
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let context = v2_context("atlas", arbitrary.as_os_str(), &[], &work, path_env(&bin));

        assert!(matches!(
            bind_native_launch(&context, "atlas-agent", &registry),
            Err(NativeBindingError::ProgramMismatch)
        ));
    }

    #[test]
    fn missing_path_is_refused_without_inventing_a_system_default() {
        let (work, _bin, executable) = executable_fixture("native-binding-missing-path");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let context = v2_context("atlas", executable.as_os_str(), &[], &work, vec![]);

        assert!(matches!(
            bind_native_launch(&context, "atlas-agent", &registry),
            Err(NativeBindingError::MissingPath)
        ));
    }

    #[test]
    fn an_empty_path_component_uses_the_submitted_cwd() {
        let work = scratch("native-binding-empty-path");
        let executable = work.join(READY.executable);
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let context = v2_context(
            "atlas",
            executable.as_os_str(),
            &[],
            &work,
            vec![env_var(OsStr::new("PATH"), OsStr::new(":"))],
        );

        let bound = bind_native_launch(&context, "atlas-agent", &registry)
            .expect("the empty component resolves against submitted cwd");

        assert_eq!(bound.program, executable.as_os_str());
    }

    #[test]
    fn a_relative_path_component_is_resolved_from_the_submitted_cwd() {
        let work = scratch("native-binding-relative-path");
        let bin = work.join("bin");
        std::fs::create_dir(&bin).expect("the relative bin exists under cwd");
        let executable = bin.join(READY.executable);
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let context = v2_context(
            "atlas",
            executable.as_os_str(),
            &[],
            &work,
            vec![env_var(OsStr::new("PATH"), OsStr::new("bin"))],
        );

        let bound = bind_native_launch(&context, "atlas-agent", &registry)
            .expect("relative PATH is interpreted from submitted cwd");

        assert_eq!(bound.program, executable.as_os_str());
    }

    #[test]
    fn invalid_utf8_path_argv_env_and_cwd_survive_binding_byte_for_byte() {
        let (work, bin, executable) = executable_fixture("native-binding-opaque");
        let opaque_cwd = work.join(OsString::from_vec(vec![b'c', b'w', b'd', 0xff]));
        let mut path_bytes = work.as_os_str().as_bytes().to_vec();
        path_bytes.extend_from_slice(b"/missing-\xff:");
        path_bytes.extend_from_slice(bin.as_os_str().as_bytes());
        let opaque_path = OsString::from_vec(path_bytes);
        let argument = OsStr::from_bytes(b"--opaque=\xfe");
        let value = OsStr::from_bytes(b"value-\xfd");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let context = v2_context(
            "atlas",
            executable.as_os_str(),
            &[argument],
            &opaque_cwd,
            vec![
                env_var(OsStr::new("PATH"), opaque_path.as_os_str()),
                env_var(OsStr::new("OPAQUE"), value),
            ],
        );

        let bound = bind_native_launch(&context, "atlas-agent", &registry)
            .expect("Unix OS bytes remain opaque");

        assert_eq!(bound.program.as_bytes(), executable.as_os_str().as_bytes());
        assert_eq!(bound.argv[0].as_bytes(), argument.as_bytes());
        assert_eq!(
            bound.cwd.as_os_str().as_bytes(),
            opaque_cwd.as_os_str().as_bytes()
        );
        assert_eq!(
            bound.env[0].1.as_bytes(),
            opaque_path.as_os_str().as_bytes()
        );
        assert_eq!(bound.env[1].1.as_bytes(), value.as_bytes());
    }

    #[test]
    fn duplicate_and_invalid_environment_names_are_refused_before_path_lookup() {
        let work = scratch("native-binding-invalid-env");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");

        let duplicate = v2_context(
            "atlas",
            OsStr::new("/missing/atlas-cli"),
            &[],
            &work,
            vec![
                env_var(OsStr::new("PATH"), OsStr::new("/missing")),
                env_var(OsStr::new("PATH"), OsStr::new("/also-missing")),
            ],
        );
        assert!(matches!(
            bind_native_launch(&duplicate, "atlas-agent", &registry),
            Err(NativeBindingError::DuplicateEnvironmentName)
        ));

        for name in [OsStr::new(""), OsStr::new("BAD=NAME")] {
            let invalid = v2_context(
                "atlas",
                OsStr::new("/missing/atlas-cli"),
                &[],
                &work,
                vec![
                    env_var(OsStr::new("PATH"), OsStr::new("/missing")),
                    env_var(name, OsStr::new("value")),
                ],
            );
            assert!(matches!(
                bind_native_launch(&invalid, "atlas-agent", &registry),
                Err(NativeBindingError::InvalidEnvironmentName)
            ));
        }
    }

    #[test]
    fn nul_in_any_native_os_value_is_refused_before_path_lookup() {
        let work = scratch("native-binding-nul");
        let registry = NativeFacadeRegistry::new(&[READY]).expect("the registry is valid");
        let cases = [
            v2_context(
                "atlas",
                OsStr::from_bytes(b"/missing/atlas\0-cli"),
                &[],
                &work,
                vec![env_var(OsStr::new("PATH"), OsStr::new("/missing"))],
            ),
            v2_context(
                "atlas",
                OsStr::new("/missing/atlas-cli"),
                &[OsStr::from_bytes(b"arg\0ument")],
                &work,
                vec![env_var(OsStr::new("PATH"), OsStr::new("/missing"))],
            ),
            v2_context(
                "atlas",
                OsStr::new("/missing/atlas-cli"),
                &[],
                Path::new(OsStr::from_bytes(b"/missing/cwd\0")),
                vec![env_var(OsStr::new("PATH"), OsStr::new("/missing"))],
            ),
            v2_context(
                "atlas",
                OsStr::new("/missing/atlas-cli"),
                &[],
                &work,
                vec![
                    env_var(OsStr::new("PATH"), OsStr::new("/missing")),
                    env_var(OsStr::from_bytes(b"NA\0ME"), OsStr::new("value")),
                ],
            ),
            v2_context(
                "atlas",
                OsStr::new("/missing/atlas-cli"),
                &[],
                &work,
                vec![
                    env_var(OsStr::new("PATH"), OsStr::new("/missing")),
                    env_var(OsStr::new("NAME"), OsStr::from_bytes(b"val\0ue")),
                ],
            ),
        ];

        for context in cases {
            assert!(matches!(
                bind_native_launch(&context, "atlas-agent", &registry),
                Err(NativeBindingError::EmbeddedNul)
            ));
        }
    }

    #[test]
    fn resolver_accepts_only_regular_executable_files() {
        let work = scratch("native-binding-file-kind");
        let non_executable_bin = work.join("plain");
        std::fs::create_dir(&non_executable_bin).expect("the bin directory exists");
        std::fs::write(non_executable_bin.join(READY.executable), b"plain\n")
            .expect("the plain file exists");
        let directory_bin = work.join("directory");
        std::fs::create_dir_all(directory_bin.join(READY.executable))
            .expect("the executable-shaped directory exists");

        for bin in [&non_executable_bin, &directory_bin] {
            let env = vec![(OsString::from("PATH"), bin.as_os_str().to_owned())];
            assert!(matches!(
                resolve_declared_executable(READY.executable, &work, &env),
                Err(NativeBindingError::ProgramNotFound(program)) if program == READY.executable
            ));
        }
    }

    #[test]
    fn resolver_returns_the_submitted_path_spelling_without_canonicalizing() {
        let work = scratch("native-binding-no-canonicalize");
        let real = work.join("real");
        std::fs::create_dir(&real).expect("the real bin exists");
        let executable = real.join(READY.executable);
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let linked = work.join("linked");
        symlink(&real, &linked).expect("the submitted PATH spelling is a symlink");
        let env = vec![(OsString::from("PATH"), linked.as_os_str().to_owned())];

        let resolved = resolve_declared_executable(READY.executable, &work, &env)
            .expect("the declared executable resolves");

        assert_eq!(resolved, linked.join(READY.executable).into_os_string());
    }
}
