use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[cfg(test)]
use marion_core::NativeFacadeDescriptor;
use marion_core::NativeFacadeRegistry;
use marion_harness::validate_native_process_values;
use marion_proto::{NativeEnvVarV1, NativeLaunchContextV2, OpaqueOsValueV1, TerminalGeometryV1};

use crate::detach::Launch;
use crate::native_binding::{NativeBindingError, resolve_declared_executable};
use crate::native_bootstrap::{
    BootstrapError, DirectNativeRequestContext, NATIVE_WIRE_VERSION, NativeBootstrapClient,
    NativeFacadeHandoff, resolve_for_cwd,
};
use crate::native_intent::SelectedNativeFacade;
use crate::native_tty::capture_process_stdio;

/// An untrusted first-token facade match. This value carries no native launch authority.
#[derive(Debug, PartialEq, Eq)]
pub struct MatchedNativeFacadeRequest {
    requested_selector: String,
    opaque_tail: Vec<OsString>,
}

impl MatchedNativeFacadeRequest {
    pub fn requested_selector(&self) -> &str {
        &self.requested_selector
    }

    pub fn opaque_tail(&self) -> &[OsString] {
        &self.opaque_tail
    }
}

/// Match argv's first token without selecting a lane or minting authorization.
pub fn match_native_facade_request<'a>(
    argv: impl IntoIterator<Item = OsString>,
    registry: &'a NativeFacadeRegistry<'a>,
) -> Option<MatchedNativeFacadeRequest> {
    let mut argv = argv.into_iter();
    let selector = argv.next()?.into_string().ok()?;
    if !selector.is_ascii() {
        return None;
    }
    registry.resolve(&selector)?;
    Some(MatchedNativeFacadeRequest {
        requested_selector: selector,
        opaque_tail: argv.collect(),
    })
}

/// Build byte-exact V2 wire state only after sealed native selection was consumed.
#[allow(
    dead_code,
    reason = "Task 2 supplies the first production authorization issuer"
)]
pub(crate) fn build_native_launch_v2(
    selection: SelectedNativeFacade<'_>,
    argv: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: PathBuf,
    geometry: TerminalGeometryV1,
) -> Result<NativeLaunchContextV2, NativeBindingError> {
    let descriptor = selection.descriptor();
    let native_lane = selection.native_lane();
    validate_native_process_values(
        std::ffi::OsStr::new(native_lane.executable()),
        &argv,
        &env,
        &cwd,
    )?;
    let program = resolve_declared_executable(native_lane.executable(), &cwd, &env)?;

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

/// Relay an authorized native launch on the operator's terminal until it ends, and say why if
/// it did not end cleanly.
///
/// The shipped binary's `authorized_native` continuation. Everything the relay does to the
/// terminal — raw mode, signal ownership, the passive cleanup bytes — is undone inside
/// `native_relay::run` before it returns, so the message below is written to a **restored**
/// terminal: a refusal printed into raw mode would be one the operator could not read.
pub fn relay_native_facade(handoff: NativeFacadeHandoff) -> ExitCode {
    match crate::native_relay::run(handoff) {
        Ok(()) => ExitCode::SUCCESS,
        Err(refusal) => {
            eprintln!("marion: {refusal}");
            ExitCode::FAILURE
        }
    }
}

/// [`NativeBootstrapClient::connect_for_cwd`], after making sure this project has a supervisor to
/// connect to — the shipped facade's connect step.
///
/// The one spawn path: [`crate::detach::ensure_supervisor`], which `marion run` and `marion
/// resume` take — dial, and start stage 1 **only** if nothing answers. `launch` is told the
/// resolved `<state>` and canonical project root and builds what stage 3 must be told, because the
/// binary to start and the provider choice are the launcher's to state, never this client's to
/// infer. The ordinary-socket connection `ensure_supervisor` returns is held until the native
/// sibling is dialed, so the supervisor has a client for every instant in between.
///
/// This composition lives on the client side, not in `native_bootstrap`: the bootstrap module is
/// what `serve` builds on, and a module `serve` depends on must not reach back through `detach`.
pub fn ensure_supervisor_then_connect(
    launch: impl FnOnce(&Path, &Path) -> Launch,
) -> Result<NativeBootstrapClient, BootstrapError> {
    let (state, cwd, paths) = resolve_for_cwd()?;
    let ensured =
        crate::detach::ensure_supervisor(&paths, &launch(&state, paths.canonical_project()))
            .map_err(|error| BootstrapError::SupervisorUnavailable(error.to_string()))?;
    let client = NativeBootstrapClient::connect(&paths, cwd)?;
    drop(ensured);
    Ok(client)
}

/// Run the untrusted native-facade match before handing unmatched argv to the legacy CLI.
///
/// Registered selectors require a foreground controlling-TTY witness, then traverse Task 2's
/// same-connection bootstrap. The server consumes authority and selects the canonical native lane;
/// this client never reads lane or readiness detail from its pre-authorization match.
pub fn dispatch_native_facade_or_legacy<'a>(
    argv: impl IntoIterator<Item = OsString>,
    registry: &'a NativeFacadeRegistry<'a>,
    connect_native: impl FnOnce() -> Result<NativeBootstrapClient, BootstrapError>,
    authorized_native: impl FnOnce(NativeFacadeHandoff) -> ExitCode,
    mut stderr: impl Write,
    legacy: impl FnOnce() -> ExitCode,
) -> ExitCode {
    let Some(request) = match_native_facade_request(argv, registry) else {
        return legacy();
    };

    let witness = match capture_process_stdio() {
        Ok(witness) => witness,
        Err(_) => {
            writeln!(
            stderr,
            "marion: native facade \"{}\" requires stdin and stdout on the same foreground controlling terminal; use 'marion run <agent-type> --prompt <text>' for structured execution",
            request.requested_selector()
        )
        .expect("write native facade refusal to stderr");
            return ExitCode::FAILURE;
        }
    };

    let connection = match connect_native() {
        Ok(connection) => connection,
        Err(error) => {
            writeln!(
                stderr,
                "marion: native facade bootstrap authorization was refused: {error}"
            )
            .expect("write native facade refusal to stderr");
            return ExitCode::FAILURE;
        }
    };

    let context = DirectNativeRequestContext::new(
        connection.canonical_project().to_path_buf(),
        connection.client_cwd().to_path_buf(),
        OsString::from(request.requested_selector()),
        request.opaque_tail().to_vec(),
        std::env::var_os("TERM").unwrap_or_default(),
        NATIVE_WIRE_VERSION,
    )
    // The operator's environment is what the vendor process must see; the constructor strips
    // marion's own reserved identity before it leaves this process.
    .with_environment(std::env::vars_os());
    let authorized = witness
        .bootstrap(connection, context)
        .and_then(|session| session.consume());
    match authorized {
        Ok(handoff) => authorized_native(handoff),
        Err(_) => {
            writeln!(
                stderr,
                "marion: native facade bootstrap authorization was refused"
            )
            .expect("write native facade refusal to stderr");
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    use marion_core::{Lane, NativeAdapterId, NativeFacadeRegistry, NativeLane, VendorIdentity};
    use marion_proto::TerminalGeometryV1;
    use marion_testsupport::scratch;

    use crate::native_intent::select_test_native;

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
    const DISABLED: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("boreal"),
        command: "boreal",
        aliases: &["bo"],
        native: Some(Lane::new(
            false,
            NativeLane::new("boreal-cli", "codex", NativeAdapterId::new("boreal-native")),
        )),
        structured: None,
    };
    const DESCRIPTORS: &[NativeFacadeDescriptor] = &[READY, DISABLED];

    fn registry() -> NativeFacadeRegistry<'static> {
        NativeFacadeRegistry::new(DESCRIPTORS).expect("the synthetic registry is valid")
    }

    fn selected_native<'a>(registry: &'a NativeFacadeRegistry<'a>) -> SelectedNativeFacade<'a> {
        select_test_native(registry, "atlas").expect("the test-only sealed native intent selects")
    }

    #[test]
    fn untrusted_match_preserves_the_opaque_tail_without_producing_selection() {
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
        let request = match_native_facade_request(argv, &registry).unwrap();

        assert_eq!(request.requested_selector(), "atlas");
        assert_eq!(request.opaque_tail(), expected);
    }

    #[test]
    fn alias_match_is_untrusted_but_builder_canonicalizes_the_authorized_selection() {
        let work = scratch("native-facade-client-v2");
        let bin = work.join("bin");
        std::fs::create_dir(&bin).expect("the fixture bin exists");
        let executable = bin.join(READY.native.unwrap().executable());
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let registry = registry();
        let matched = match_native_facade_request(
            [OsString::from("at"), OsString::from("--opaque-tail")],
            &registry,
        )
        .expect("the alias matches without authorizing");
        let geometry = TerminalGeometryV1 {
            cols: 103,
            rows: 41,
            xpixel: 7,
            ypixel: 11,
        };

        let context = build_native_launch_v2(
            selected_native(&registry),
            matched.opaque_tail,
            vec![(OsString::from("PATH"), bin.as_os_str().to_owned())],
            work.to_path_buf(),
            geometry.clone(),
        )
        .expect("the selected descriptor's executable resolves");

        assert_eq!(context.facade_command, "atlas");
        assert_eq!(context.program.to_os_string().unwrap(), executable);
        assert_eq!(context.argv[0].to_os_string().unwrap(), "--opaque-tail");
        assert_eq!(context.cwd.to_os_string().unwrap(), work.as_os_str());
        assert_eq!(context.geometry, geometry);
    }

    #[test]
    fn only_the_first_registered_token_matches_without_exposing_lane_policy() {
        let registry = registry();
        assert!(
            match_native_facade_request(
                [OsString::from("unknown"), OsString::from("atlas")],
                &registry,
            )
            .is_none()
        );
        for selector in [
            "run", "attach", "tree", "mcp", "doctor", "help", "version", "-h", "--help", "unknown",
            "lambda",
        ] {
            assert!(
                match_native_facade_request([OsString::from(selector)], &registry).is_none(),
                "{selector:?} escaped the legacy parser"
            );
        }
        for selector in ["boreal", "bo"] {
            assert_eq!(
                match_native_facade_request([OsString::from(selector)], &registry)
                    .unwrap()
                    .requested_selector(),
                selector
            );
        }
        assert!(match_native_facade_request(Vec::<OsString>::new(), &registry).is_none());
    }

    #[test]
    fn invalid_utf8_selector_returns_control_but_tail_stays_byte_exact() {
        let registry = registry();
        assert!(
            match_native_facade_request([OsString::from_vec(vec![0xff, 0x80, b'x'])], &registry,)
                .is_none()
        );

        let invalid = OsString::from_vec(vec![0xff, 0x80, b'x']);
        let request =
            match_native_facade_request([OsString::from("atlas"), invalid.clone()], &registry)
                .unwrap();
        assert_eq!(request.opaque_tail(), [invalid]);
    }
}
