//! Raw ordinary-RPC launch state cannot call the authorized native binder.
//!
//! ```compile_fail
//! use marion_core::production_native_facades;
//! use marion_proto::NativeLaunchContext;
//! use marion_supervisor::native_binding::bind_native_launch;
//!
//! let registry = production_native_facades();
//! let raw: &NativeLaunchContext = todo!();
//! let _forged = bind_native_launch(raw, "codex-impl", &registry);
//! ```

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use marion_core::{AgentType, NativeFacadeDescriptor};
use marion_harness::{
    NativeInjectionError, NativeProcessBase, NativeTerminalGeometry, validate_native_process_values,
};
use marion_proto::{
    NativeLaunchContext, NativeLaunchContextV2, NativeOsValueConversionError, TerminalGeometryV1,
};

use crate::native_intent::ReadyNativeFacade;

/// A validated V2 launch bound to one ready facade and its exact declared executable.
#[derive(Debug, PartialEq, Eq)]
pub struct BoundNativeLaunch<'a> {
    pub descriptor: &'a NativeFacadeDescriptor,
    pub agent_type: &'a AgentType,
    pub program: OsString,
    pub argv: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub geometry: TerminalGeometryV1,
}

impl BoundNativeLaunch<'_> {
    /// Consume this binding by moving its exact process values into harness-neutral state,
    /// intentionally discarding the validated descriptor identity without revalidation.
    pub fn into_process_base(self) -> NativeProcessBase {
        let Self {
            descriptor: _,
            agent_type: _,
            program,
            argv,
            env,
            cwd,
            geometry,
        } = self;
        NativeProcessBase {
            program,
            user_argv: argv,
            env,
            cwd,
            geometry: NativeTerminalGeometry {
                cols: geometry.cols,
                rows: geometry.rows,
                xpixel: geometry.xpixel,
                ypixel: geometry.ypixel,
            },
        }
    }
}

/// Why byte-exact native launch state could not be bound to a declared facade.
#[derive(Debug, thiserror::Error)]
pub enum NativeBindingError {
    #[error("native launch V1 carries no facade selector and cannot be bound")]
    SelectorMissingInV1,
    #[error("native facade command {0:?} is not a ready canonical primary")]
    NonCanonicalFacadeCommand(String),
    #[error("raw V2 native launch requires the native bootstrap authorization path")]
    RawV2RequiresNativeBootstrap,
    #[error("native launch environment has no PATH")]
    MissingPath,
    #[error(transparent)]
    InvalidProcess(#[from] NativeInjectionError),
    #[error("declared executable {0:?} was not found on the submitted PATH")]
    ProgramNotFound(String),
    #[error("submitted native program does not equal the declared executable resolution")]
    ProgramMismatch,
    #[error(transparent)]
    UnsupportedPlatform(#[from] NativeOsValueConversionError),
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

        validate_native_process_values(OsStr::new(executable), &[], env, cwd)?;

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
    validate_native_process_values(&program, &argv, &env, &cwd)?;
    Ok(ConvertedNativeLaunch {
        program,
        argv,
        cwd,
        env,
    })
}

/// Return the refusal for ordinary RPC native state without descriptor resolution.
///
/// This return type deliberately has no success variant: raw V1/V2 compatibility input can never
/// manufacture a [`BoundNativeLaunch`].
pub fn refuse_untrusted_native_launch(context: &NativeLaunchContext) -> NativeBindingError {
    match context {
        NativeLaunchContext::V1(_) => NativeBindingError::SelectorMissingInV1,
        NativeLaunchContext::V2(_) => NativeBindingError::RawV2RequiresNativeBootstrap,
    }
}

/// Bind V2 state only to an already-authorized native selection and current readiness evidence.
///
/// There is intentionally no registry argument and no caller-supplied agent identity: selection
/// resolved both once, and this function cannot reselect them from wire data.
pub fn bind_native_launch<'a>(
    selection: ReadyNativeFacade<'a>,
    context: &NativeLaunchContextV2,
) -> Result<BoundNativeLaunch<'a>, NativeBindingError> {
    let descriptor = selection.descriptor();
    let native_lane = selection.native_lane();
    if context.facade_command != descriptor.command {
        return Err(NativeBindingError::NonCanonicalFacadeCommand(
            context.facade_command.clone(),
        ));
    }
    let ConvertedNativeLaunch {
        program,
        argv,
        cwd,
        env,
    } = convert_v2(context)?;

    let declared = resolve_declared_executable(native_lane.executable(), &cwd, &env)?;
    if program != declared {
        return Err(NativeBindingError::ProgramMismatch);
    }

    Ok(BoundNativeLaunch {
        descriptor,
        agent_type: native_lane.agent_type(),
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
        Lane, LaneReadiness, NativeAdapterId, NativeFacadeDescriptor, NativeFacadeRegistry,
        NativeLane, StructuredAdapterId, StructuredAgentIdentity, StructuredControl,
        StructuredLane, VendorIdentity, production_native_facades,
    };
    use marion_proto::{
        NativeEnvVarV1, NativeLaunchContext, NativeLaunchContextV1, NativeLaunchContextV2,
        OpaqueOsValueV1, TerminalGeometryV1,
    };
    use marion_testsupport::{Scratch, scratch};

    use crate::native_intent::{ReadyNativeFacade, ready_test_native, select_test_native};

    use super::*;

    const READY: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &["at"],
        native: Some(Lane::new(
            true,
            NativeLane::new("atlas-cli", "codex", NativeAdapterId::new("atlas-native")),
        )),
        structured: None,
    };
    const PLANNED: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("boreal"),
        command: "boreal",
        aliases: &["bo"],
        native: Some(Lane::new(
            false,
            NativeLane::new("boreal-cli", "codex", NativeAdapterId::new("boreal-native")),
        )),
        structured: None,
    };

    fn ready_readiness() -> LaneReadiness {
        LaneReadiness::evaluate(true, true, true, true, true)
    }

    fn ready_selection<'a>(
        registry: &'a NativeFacadeRegistry<'a>,
        selector: &str,
    ) -> ReadyNativeFacade<'a> {
        let selected = select_test_native(registry, selector)
            .expect("the test-only authorized selector resolves a native lane");
        ready_test_native(selected, ready_readiness())
            .expect("the test fixture supplies complete current readiness")
    }

    fn bind_ready<'a>(
        context: &NativeLaunchContext,
        selection: ReadyNativeFacade<'a>,
    ) -> Result<BoundNativeLaunch<'a>, NativeBindingError> {
        let NativeLaunchContext::V2(context) = context else {
            return Err(refuse_untrusted_native_launch(context));
        };
        super::bind_native_launch(selection, context)
    }

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
        let executable = bin.join(READY.native.unwrap().executable());
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
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
        let context = v2_context(
            "atlas",
            executable.as_os_str(),
            &[OsStr::new("--native")],
            &bin,
            path_env(&bin),
        );

        let bound = bind_ready(&context, ready_selection(&registry, "atlas"))
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
        let context = NativeLaunchContext::V1(NativeLaunchContextV1::new(
            opaque(OsStr::from_bytes(b"missing\0program")),
            vec![],
            opaque(OsStr::new("/missing/cwd")),
            vec![],
            geometry(),
        ));

        assert!(matches!(
            refuse_untrusted_native_launch(&context),
            NativeBindingError::SelectorMissingInV1
        ));
    }

    #[test]
    fn raw_v2_compatibility_input_has_no_success_path() {
        let (work, bin, executable) = executable_fixture("native-binding-raw-v2");
        let context = v2_context("atlas", executable.as_os_str(), &[], &work, path_env(&bin));

        assert!(matches!(
            refuse_untrusted_native_launch(&context),
            NativeBindingError::RawV2RequiresNativeBootstrap
        ));
    }

    #[test]
    fn blocked_current_readiness_cannot_bind_an_authorized_selection() {
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
        let selected = select_test_native(&registry, "atlas").unwrap();
        let blocked = LaneReadiness::evaluate(true, true, true, true, false);

        assert!(ready_test_native(selected, blocked).is_none());
    }

    #[test]
    fn an_alias_is_not_accepted_as_a_wire_selector() {
        let (work, bin, executable) = executable_fixture("native-binding-alias");
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
        let alias_context = NativeLaunchContext::V2(NativeLaunchContextV2::new(
            "at".into(),
            opaque(executable.as_os_str()),
            vec![],
            opaque(work.as_os_str()),
            path_env(&bin),
            geometry(),
        ));

        assert!(matches!(
            bind_ready(&alias_context, ready_selection(&registry, "at")),
            Err(NativeBindingError::NonCanonicalFacadeCommand(command)) if command == "at"
        ));
    }

    #[test]
    fn unknown_planned_and_empty_production_commands_are_unavailable() {
        let descriptors = [READY, PLANNED];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");

        for command in ["unknown", "boreal", "bo"] {
            assert!(select_test_native(&registry, command).is_none());
        }

        assert!(select_test_native(&production_native_facades(), "atlas").is_none());
    }

    #[test]
    fn bound_launch_carries_the_registry_canonical_agent_type_without_caller_input() {
        let (work, bin, executable) = executable_fixture("native-binding-agent");
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
        let context = v2_context("atlas", executable.as_os_str(), &[], &work, path_env(&bin));
        let NativeLaunchContext::V2(context) = context else {
            unreachable!()
        };
        let bound = super::bind_native_launch(ready_selection(&registry, "atlas"), &context)
            .expect("authorized current evidence binds");

        assert_eq!(bound.agent_type.name, "codex-impl");
    }

    #[test]
    fn a_structured_only_descriptor_does_not_enter_the_native_pty_kernel() {
        let descriptor = NativeFacadeDescriptor {
            native: None,
            structured: Some(Lane::new(
                true,
                StructuredLane::new(
                    StructuredAgentIdentity::new("atlas-acp", 1),
                    StructuredControl::Acp,
                    StructuredAdapterId::new("atlas-structured"),
                ),
            )),
            ..READY
        };
        let registry = NativeFacadeRegistry::new(std::slice::from_ref(&descriptor))
            .expect("the registry is valid");

        assert!(select_test_native(&registry, "atlas").is_none());
    }

    #[test]
    fn a_right_basename_at_an_arbitrary_path_does_not_bind() {
        let (work, bin, _executable) = executable_fixture("native-binding-program-mismatch");
        let arbitrary = work
            .join("elsewhere")
            .join(READY.native.unwrap().executable());
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
        let context = v2_context("atlas", arbitrary.as_os_str(), &[], &work, path_env(&bin));

        assert!(matches!(
            bind_ready(&context, ready_selection(&registry, "atlas")),
            Err(NativeBindingError::ProgramMismatch)
        ));
    }

    #[test]
    fn missing_path_is_refused_without_inventing_a_system_default() {
        let (work, _bin, executable) = executable_fixture("native-binding-missing-path");
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
        let context = v2_context("atlas", executable.as_os_str(), &[], &work, vec![]);

        assert!(matches!(
            bind_ready(&context, ready_selection(&registry, "atlas")),
            Err(NativeBindingError::MissingPath)
        ));
    }

    #[test]
    fn an_empty_path_component_uses_the_submitted_cwd() {
        let work = scratch("native-binding-empty-path");
        let executable = work.join(READY.native.unwrap().executable());
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
        let context = v2_context(
            "atlas",
            executable.as_os_str(),
            &[],
            &work,
            vec![env_var(OsStr::new("PATH"), OsStr::new(":"))],
        );

        let bound = bind_ready(&context, ready_selection(&registry, "atlas"))
            .expect("the empty component resolves against submitted cwd");

        assert_eq!(bound.program, executable.as_os_str());
    }

    #[test]
    fn a_relative_path_component_is_resolved_from_the_submitted_cwd() {
        let work = scratch("native-binding-relative-path");
        let bin = work.join("bin");
        std::fs::create_dir(&bin).expect("the relative bin exists under cwd");
        let executable = bin.join(READY.native.unwrap().executable());
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
        let context = v2_context(
            "atlas",
            executable.as_os_str(),
            &[],
            &work,
            vec![env_var(OsStr::new("PATH"), OsStr::new("bin"))],
        );

        let bound = bind_ready(&context, ready_selection(&registry, "atlas"))
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
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
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

        let bound = bind_ready(&context, ready_selection(&registry, "atlas"))
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
    fn duplicate_and_invalid_environment_names_are_refused_after_authorized_selection() {
        let work = scratch("native-binding-invalid-env");
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");

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
            bind_ready(&duplicate, ready_selection(&registry, "atlas")),
            Err(NativeBindingError::InvalidProcess(
                NativeInjectionError::DuplicateEnvironmentName
            ))
        ));

        let empty = v2_context(
            "atlas",
            OsStr::new("/missing/atlas-cli"),
            &[],
            &work,
            vec![
                env_var(OsStr::new("PATH"), OsStr::new("/missing")),
                env_var(OsStr::new(""), OsStr::new("value")),
            ],
        );
        assert!(matches!(
            bind_ready(&empty, ready_selection(&registry, "atlas")),
            Err(NativeBindingError::InvalidProcess(
                NativeInjectionError::EmptyEnvironmentName
            ))
        ));

        let equals = v2_context(
            "atlas",
            OsStr::new("/missing/atlas-cli"),
            &[],
            &work,
            vec![
                env_var(OsStr::new("PATH"), OsStr::new("/missing")),
                env_var(OsStr::new("BAD=NAME"), OsStr::new("value")),
            ],
        );
        assert!(matches!(
            bind_ready(&equals, ready_selection(&registry, "atlas")),
            Err(NativeBindingError::InvalidProcess(
                NativeInjectionError::InvalidEnvironmentName
            ))
        ));
    }

    #[test]
    fn nul_in_any_native_os_value_is_refused_after_authorized_selection() {
        let work = scratch("native-binding-nul");
        let descriptors = [READY];
        let registry = NativeFacadeRegistry::new(&descriptors).expect("the registry is valid");
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
                bind_ready(&context, ready_selection(&registry, "atlas")),
                Err(NativeBindingError::InvalidProcess(
                    NativeInjectionError::EmbeddedNul
                ))
            ));
        }
    }

    #[test]
    fn resolver_accepts_only_regular_executable_files() {
        let work = scratch("native-binding-file-kind");
        let non_executable_bin = work.join("plain");
        std::fs::create_dir(&non_executable_bin).expect("the bin directory exists");
        std::fs::write(
            non_executable_bin.join(READY.native.unwrap().executable()),
            b"plain\n",
        )
        .expect("the plain file exists");
        let directory_bin = work.join("directory");
        std::fs::create_dir_all(directory_bin.join(READY.native.unwrap().executable()))
            .expect("the executable-shaped directory exists");

        for bin in [&non_executable_bin, &directory_bin] {
            let env = vec![(OsString::from("PATH"), bin.as_os_str().to_owned())];
            assert!(matches!(
                resolve_declared_executable(READY.native.unwrap().executable(), &work, &env),
                Err(NativeBindingError::ProgramNotFound(program))
                    if program == READY.native.unwrap().executable()
            ));
        }
    }

    #[test]
    fn resolver_returns_the_submitted_path_spelling_without_canonicalizing() {
        let work = scratch("native-binding-no-canonicalize");
        let real = work.join("real");
        std::fs::create_dir(&real).expect("the real bin exists");
        let executable = real.join(READY.native.unwrap().executable());
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let linked = work.join("linked");
        symlink(&real, &linked).expect("the submitted PATH spelling is a symlink");
        let env = vec![(OsString::from("PATH"), linked.as_os_str().to_owned())];

        let resolved = resolve_declared_executable(READY.native.unwrap().executable(), &work, &env)
            .expect("the declared executable resolves");

        assert_eq!(
            resolved,
            linked
                .join(READY.native.unwrap().executable())
                .into_os_string()
        );
    }
}
