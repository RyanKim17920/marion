use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use marion_core::{NativeFacadeDescriptor, NativeFacadeRegistry};
use marion_proto::{NativeEnvVarV1, NativeLaunchContextV2, OpaqueOsValueV1, TerminalGeometryV1};

use crate::native_binding::{
    NativeBindingError, resolve_declared_executable, validate_launch_values,
};

/// One ready native facade selected by argv's first token.
#[derive(Debug, PartialEq, Eq)]
pub struct NativeFacadeInvocation<'a> {
    pub descriptor: &'a NativeFacadeDescriptor,
    pub argv: Vec<OsString>,
}

/// Build byte-exact V2 wire state from an invocation already resolved through the facade registry.
pub fn build_native_launch_v2(
    invocation: NativeFacadeInvocation<'_>,
    env: Vec<(OsString, OsString)>,
    cwd: PathBuf,
    geometry: TerminalGeometryV1,
) -> Result<NativeLaunchContextV2, NativeBindingError> {
    let NativeFacadeInvocation { descriptor, argv } = invocation;
    validate_launch_values(
        std::ffi::OsStr::new(descriptor.executable),
        &argv,
        &cwd,
        &env,
    )?;
    let program = resolve_declared_executable(descriptor.executable, &cwd, &env)?;

    Ok(NativeLaunchContextV2::new(
        descriptor.command.into(),
        OpaqueOsValueV1::from_os_str(&program)?,
        argv.iter()
            .map(|argument| OpaqueOsValueV1::from_os_str(argument))
            .collect::<Result<Vec<_>, _>>()?,
        OpaqueOsValueV1::from_os_str(cwd.as_os_str())?,
        env.iter()
            .map(|(name, value)| {
                Ok(NativeEnvVarV1 {
                    name: OpaqueOsValueV1::from_os_str(name)?,
                    value: OpaqueOsValueV1::from_os_str(value)?,
                })
            })
            .collect::<Result<Vec<_>, marion_proto::NativeOsValueConversionError>>()?,
        geometry,
    ))
}

/// Resolve only argv's first token and return a ready facade's remaining OS arguments untouched.
pub fn resolve_native_invocation<'a>(
    argv: impl IntoIterator<Item = OsString>,
    registry: &'a NativeFacadeRegistry<'a>,
) -> Option<NativeFacadeInvocation<'a>> {
    let mut argv = argv.into_iter();
    let selector = argv.next()?.into_string().ok()?;
    if !selector.is_ascii() {
        return None;
    }
    let descriptor = registry.resolve_for_launch(&selector)?;
    Some(NativeFacadeInvocation {
        descriptor,
        argv: argv.collect(),
    })
}

/// Run the native-facade probe before handing unmatched argv to the legacy CLI.
///
/// This is the user-facing binary's top-level dispatch boundary. Parameterizing argv and the
/// validated registry lets tests exercise a ready descriptor while the production registry stays
/// empty, and parameterizing the legacy continuation makes the required ordering observable.
pub fn dispatch_native_facade_or_legacy<'a>(
    argv: impl IntoIterator<Item = OsString>,
    registry: &'a NativeFacadeRegistry<'a>,
    mut stderr: impl Write,
    legacy: impl FnOnce() -> ExitCode,
) -> ExitCode {
    let Some(invocation) = resolve_native_invocation(argv, registry) else {
        return legacy();
    };

    writeln!(
        stderr,
        "marion: native facade transport is not ready for {:?}",
        invocation.descriptor.command
    )
    .expect("write native facade refusal to stderr");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use marion_core::{
        NativeFacadeDescriptor, NativeFacadeReadiness, NativeFacadeRegistry, NativeFacadeTransport,
    };
    use marion_proto::TerminalGeometryV1;
    use marion_testsupport::scratch;

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
    const DESCRIPTORS: &[NativeFacadeDescriptor] = &[READY, PLANNED];

    fn registry() -> NativeFacadeRegistry<'static> {
        NativeFacadeRegistry::new(DESCRIPTORS).expect("the synthetic registry is valid")
    }

    #[test]
    fn a_ready_primary_consumes_only_the_selector_and_preserves_its_opaque_tail() {
        let expected = vec![
            OsString::from("--help"),
            OsString::from("--version"),
            OsString::from("--"),
            OsString::from("position"),
            OsString::from(""),
        ];
        let argv = std::iter::once(OsString::from("atlas"))
            .chain(expected.clone())
            .collect::<Vec<_>>();

        let registry = registry();
        let invocation =
            resolve_native_invocation(argv, &registry).expect("the ready facade resolves");

        assert_eq!(invocation.descriptor, &READY);
        assert_eq!(invocation.argv, expected);
    }

    #[test]
    fn a_ready_alias_routes_to_the_same_descriptor() {
        let registry = registry();
        let invocation = resolve_native_invocation([OsString::from("at")], &registry)
            .expect("the ready alias resolves");

        assert_eq!(invocation.descriptor, &READY);
        assert!(invocation.argv.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn client_constructor_canonicalizes_an_alias_through_its_resolved_descriptor() {
        let work = scratch("native-facade-client-v2");
        let bin = work.join("bin");
        std::fs::create_dir(&bin).expect("the fixture bin exists");
        let executable = bin.join(READY.executable);
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let registry = registry();
        let invocation = resolve_native_invocation(
            [OsString::from("at"), OsString::from("--opaque-tail")],
            &registry,
        )
        .expect("the ready alias resolves before construction");
        let geometry = TerminalGeometryV1 {
            cols: 103,
            rows: 41,
            xpixel: 7,
            ypixel: 11,
        };

        let context = build_native_launch_v2(
            invocation,
            vec![(OsString::from("PATH"), bin.as_os_str().to_owned())],
            work.to_path_buf(),
            geometry.clone(),
        )
        .expect("the descriptor's declared executable resolves");

        assert_eq!(context.facade_command, "atlas");
        assert_eq!(context.program.to_os_string().unwrap(), executable);
        assert_eq!(
            context.argv[0].to_os_string().unwrap(),
            OsString::from("--opaque-tail")
        );
        assert_eq!(context.cwd.to_os_string().unwrap(), work.as_os_str());
        assert_eq!(context.geometry, geometry);
    }

    #[test]
    fn only_the_first_token_can_select_a_facade() {
        let registry = registry();
        let invocation = resolve_native_invocation(
            [OsString::from("unknown"), OsString::from("atlas")],
            &registry,
        );

        assert!(invocation.is_none());
    }

    #[test]
    fn legacy_reserved_unknown_and_planned_selectors_return_control() {
        for selector in [
            "run", "attach", "tree", "mcp", "doctor", "help", "version", "-h", "--help", "unknown",
            "boreal", "bo", "λ",
        ] {
            let registry = registry();
            let invocation = resolve_native_invocation([OsString::from(selector)], &registry);
            assert!(
                invocation.is_none(),
                "{selector:?} escaped the legacy parser"
            );
        }
        assert!(resolve_native_invocation(Vec::<OsString>::new(), &registry()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn invalid_utf8_in_the_selector_returns_control_to_the_legacy_parser() {
        use std::os::unix::ffi::OsStringExt;

        let registry = registry();
        let invocation =
            resolve_native_invocation([OsString::from_vec(vec![0xff, 0x80, b'x'])], &registry);

        assert!(invocation.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_ready_facade_preserves_invalid_utf8_in_its_tail_byte_for_byte() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![0xff, 0x80, b'x']);
        let registry = registry();
        let invocation =
            resolve_native_invocation([OsString::from("atlas"), invalid.clone()], &registry)
                .expect("the ready facade resolves");

        assert_eq!(invocation.argv, vec![invalid]);
    }
}
