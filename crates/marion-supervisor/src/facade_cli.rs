use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use marion_core::{NativeFacadeDescriptor, NativeFacadeRegistry};

/// One ready native facade selected by argv's first token.
#[derive(Debug, PartialEq, Eq)]
pub struct NativeFacadeInvocation<'a> {
    pub descriptor: &'a NativeFacadeDescriptor,
    pub argv: Vec<OsString>,
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

    use marion_core::{NativeFacadeDescriptor, NativeFacadeReadiness, NativeFacadeRegistry};

    use super::*;

    const READY: NativeFacadeDescriptor = NativeFacadeDescriptor {
        command: "atlas",
        aliases: &["at"],
        agent_type: "atlas-agent",
        readiness: NativeFacadeReadiness::Ready,
    };
    const PLANNED: NativeFacadeDescriptor = NativeFacadeDescriptor {
        command: "boreal",
        aliases: &["bo"],
        agent_type: "boreal-agent",
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
