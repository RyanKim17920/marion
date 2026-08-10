#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use marion_core::{
    NativeFacadeDescriptor, NativeFacadeReadiness, NativeFacadeRegistry, NativeFacadeTransport,
    production_native_facades,
};
use marion_harness::mcp_bridge::MarionMcpBridge;
use marion_harness::{
    NativeDocument, NativeEnvironmentView, NativeInjection, NativeInjectionAdapter,
    NativeInjectionError, NativeNodeContext, assemble_native,
};
use marion_proto::{
    NativeEnvVarV1, NativeLaunchContext, NativeLaunchContextV2, OpaqueOsValueV1, TerminalGeometryV1,
};
use marion_supervisor::native_binding::{NativeBindingError, bind_native_launch};
use marion_testsupport::{Scratch, scratch};

const READY: NativeFacadeDescriptor = NativeFacadeDescriptor {
    command: "atlas",
    aliases: &["at"],
    executable: "atlas-cli",
    agent_type: "atlas-agent",
    transport: NativeFacadeTransport::TransparentPty,
    readiness: NativeFacadeReadiness::Ready,
};

fn opaque(value: &OsStr) -> OpaqueOsValueV1 {
    OpaqueOsValueV1::from_os_str(value).expect("Unix preserves native launch bytes")
}

fn env_var(name: &OsStr, value: &OsStr) -> NativeEnvVarV1 {
    NativeEnvVarV1 {
        name: opaque(name),
        value: opaque(value),
    }
}

fn geometry() -> TerminalGeometryV1 {
    TerminalGeometryV1 {
        cols: 137,
        rows: 41,
        xpixel: 13,
        ypixel: 17,
    }
}

fn executable_fixture(tag: &str) -> (Scratch, PathBuf, PathBuf) {
    let work = scratch(tag);
    let bin = work.join("bin");
    std::fs::create_dir(&bin).expect("the isolated fixture bin exists");
    let executable = bin.join(READY.executable);
    std::fs::write(&executable, b"inert synthetic executable\n")
        .expect("the inert executable fixture is written");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("the inert executable fixture is executable");
    (work, bin, executable)
}

fn v2_context(
    program: &OsStr,
    argv: &[&OsStr],
    cwd: &Path,
    env: Vec<NativeEnvVarV1>,
) -> NativeLaunchContext {
    NativeLaunchContext::V2(NativeLaunchContextV2::new(
        READY.command.into(),
        opaque(program),
        argv.iter().map(|argument| opaque(argument)).collect(),
        opaque(cwd.as_os_str()),
        env,
        geometry(),
    ))
}

fn bridge() -> MarionMcpBridge {
    MarionMcpBridge {
        program: OsString::from("/synthetic/marion-supervisor"),
        args: vec![OsString::from("mcp")],
        env: vec![(OsString::from("MARION_AGENT_ID"), OsString::from("root"))],
    }
}

struct SyntheticAdapter;

impl NativeInjectionAdapter for SyntheticAdapter {
    fn prepare_native(
        &self,
        context: &NativeNodeContext<'_>,
    ) -> Result<NativeInjection, NativeInjectionError> {
        if context.bridge.program.as_os_str() != OsStr::new("/synthetic/marion-supervisor")
            || context.bridge.args != [OsString::from("mcp")]
            || context.bridge.env != [(OsString::from("MARION_AGENT_ID"), OsString::from("root"))]
        {
            return Err(NativeInjectionError::Adapter(
                "unexpected synthetic bridge declaration".into(),
            ));
        }
        if context.allowed_marion_tools != ["spawn_agent", "send_message"] {
            return Err(NativeInjectionError::Adapter(
                "unexpected Marion tool policy".into(),
            ));
        }
        let source = context
            .environment
            .get(OsStr::new("SOURCE"))
            .ok_or_else(|| NativeInjectionError::Adapter("missing SOURCE".into()))?;

        Ok(NativeInjection {
            argv_prefix: vec![OsString::from_vec(vec![b'p', b'r', b'e', 0xf6])],
            env_overlay: vec![(OsString::from("REPLACE"), source.to_owned())],
            documents: vec![NativeDocument {
                path: context.document_dir.join("synthetic.json"),
                contents: b"{\"adapter\":\"synthetic\"}".to_vec(),
            }],
        })
    }
}

#[test]
fn binding_and_assembly_preserve_every_indexed_native_value_without_launching() {
    let (work, bin, executable) = executable_fixture("native-injection-binding");
    let document_dir = work.join("documents-must-remain-absent");
    assert!(
        !document_dir
            .try_exists()
            .expect("document path absence is observable before preparation")
    );

    let first_tail = OsString::from_vec(vec![b't', b'a', b'i', b'l', 0xff]);
    let boundary = OsString::from("--");
    let empty_tail = OsString::new();
    let last_tail = OsString::from_vec(vec![0x80, b'x']);
    let untouched_value = OsString::from_vec(vec![b'u', b'n', b't', 0xfb]);
    let source_value = OsString::from_vec(vec![b's', b'r', b'c', 0xfc]);
    let invalid_name = OsString::from_vec(vec![b'O', b'P', 0xfd]);
    let invalid_value = OsString::from_vec(vec![b'v', b'a', b'l', 0xfa]);
    let opaque_cwd = work.join(OsString::from_vec(vec![b'c', b'w', b'd', 0xf9]));
    let context = v2_context(
        executable.as_os_str(),
        &[
            first_tail.as_os_str(),
            boundary.as_os_str(),
            empty_tail.as_os_str(),
            last_tail.as_os_str(),
        ],
        &opaque_cwd,
        vec![
            env_var(OsStr::new("PATH"), bin.as_os_str()),
            env_var(OsStr::new("UNTOUCHED"), untouched_value.as_os_str()),
            env_var(OsStr::new("SOURCE"), source_value.as_os_str()),
            env_var(OsStr::new("REPLACE"), OsStr::new("old")),
            env_var(invalid_name.as_os_str(), invalid_value.as_os_str()),
            env_var(OsStr::new("MARION_AGENT_ID"), OsStr::new("forged")),
        ],
    );
    let descriptors = [READY];
    let registry =
        NativeFacadeRegistry::new(&descriptors).expect("the synthetic registry is valid");
    let base = bind_native_launch(&context, READY.agent_type, &registry)
        .expect("the exact synthetic facade launch binds")
        .into_process_base();
    let bridge = bridge();
    let tools = ["spawn_agent", "send_message"];
    let injection = SyntheticAdapter
        .prepare_native(&NativeNodeContext {
            bridge: &bridge,
            document_dir: &document_dir,
            allowed_marion_tools: &tools,
            environment: NativeEnvironmentView::validate(&base.env)
                .expect("the bound environment is a valid exact adapter view"),
        })
        .expect("the synthetic adapter prepares an injection");
    let prepared = assemble_native(base, injection).expect("the native launch assembles");

    assert_eq!(
        prepared.invocation.program.as_os_str().as_bytes(),
        executable.as_os_str().as_bytes()
    );
    assert_eq!(
        prepared.invocation.args[0].as_os_str().as_bytes(),
        &[b'p', b'r', b'e', 0xf6]
    );
    assert_eq!(
        prepared.invocation.args[1].as_os_str().as_bytes(),
        &[b't', b'a', b'i', b'l', 0xff]
    );
    assert_eq!(prepared.invocation.args[2].as_os_str().as_bytes(), b"--");
    assert_eq!(prepared.invocation.args[3].as_os_str().as_bytes(), b"");
    assert_eq!(
        prepared.invocation.args[4].as_os_str().as_bytes(),
        &[0x80, b'x']
    );
    assert_eq!(prepared.invocation.args.len(), 5);

    assert_eq!(prepared.invocation.env.len(), 5);
    assert_eq!(prepared.invocation.env[0].0, OsStr::new("PATH"));
    assert_eq!(
        prepared.invocation.env[0].1.as_os_str().as_bytes(),
        bin.as_os_str().as_bytes()
    );
    assert_eq!(prepared.invocation.env[1].0, OsStr::new("UNTOUCHED"));
    assert_eq!(
        prepared.invocation.env[1].1.as_os_str().as_bytes(),
        &[b'u', b'n', b't', 0xfb]
    );
    assert_eq!(prepared.invocation.env[2].0, OsStr::new("SOURCE"));
    assert_eq!(
        prepared.invocation.env[2].1.as_os_str().as_bytes(),
        &[b's', b'r', b'c', 0xfc]
    );
    assert_eq!(prepared.invocation.env[3].0, OsStr::new("REPLACE"));
    assert_eq!(
        prepared.invocation.env[3].1.as_os_str().as_bytes(),
        &[b's', b'r', b'c', 0xfc]
    );
    assert_eq!(
        prepared.invocation.env[4].0.as_os_str().as_bytes(),
        &[b'O', b'P', 0xfd]
    );
    assert_eq!(
        prepared.invocation.env[4].1.as_os_str().as_bytes(),
        &[b'v', b'a', b'l', 0xfa]
    );
    assert!(
        prepared
            .invocation
            .env
            .iter()
            .all(|(name, _)| name.as_os_str() != OsStr::new("MARION_AGENT_ID"))
    );
    assert_eq!(
        prepared.invocation.cwd.as_os_str().as_bytes(),
        opaque_cwd.as_os_str().as_bytes()
    );
    assert_eq!(prepared.invocation.geometry.cols, 137);
    assert_eq!(prepared.invocation.geometry.rows, 41);
    assert_eq!(prepared.invocation.geometry.xpixel, 13);
    assert_eq!(prepared.invocation.geometry.ypixel, 17);
    assert!(prepared.invocation.requires_env_clear());

    assert_eq!(prepared.documents.len(), 1);
    assert_eq!(
        prepared.documents[0].path,
        document_dir.join("synthetic.json")
    );
    assert_eq!(
        prepared.documents[0].contents,
        b"{\"adapter\":\"synthetic\"}"
    );
    assert!(
        !document_dir
            .try_exists()
            .expect("document path absence is observable after assembly")
    );
    assert!(production_native_facades().ready_commands().is_empty());
}

fn prepare_only_after_binding(
    context: &NativeLaunchContext,
    registry: &NativeFacadeRegistry<'_>,
    document_dir: &Path,
    adapter_calls: &AtomicUsize,
) -> Result<(), NativeBindingError> {
    let base = bind_native_launch(context, READY.agent_type, registry)?.into_process_base();
    adapter_calls.fetch_add(1, Ordering::SeqCst);
    let bridge = bridge();
    let tools = ["spawn_agent", "send_message"];
    let _injection = SyntheticAdapter
        .prepare_native(&NativeNodeContext {
            bridge: &bridge,
            document_dir,
            allowed_marion_tools: &tools,
            environment: NativeEnvironmentView::validate(&base.env)
                .expect("the bound environment is a valid exact adapter view"),
        })
        .expect("a bound launch can reach the synthetic adapter");
    Ok(())
}

#[test]
fn typed_transport_is_refused_before_adapter_preparation() {
    let (work, bin, executable) = executable_fixture("native-injection-typed-transport");
    let typed = NativeFacadeDescriptor {
        transport: NativeFacadeTransport::TypedAcp,
        ..READY
    };
    let descriptors = [typed];
    let registry =
        NativeFacadeRegistry::new(&descriptors).expect("the synthetic registry is valid");
    let context = v2_context(
        executable.as_os_str(),
        &[],
        &work,
        vec![
            env_var(OsStr::new("PATH"), bin.as_os_str()),
            env_var(OsStr::new("SOURCE"), OsStr::new("source")),
            env_var(OsStr::new("REPLACE"), OsStr::new("old")),
        ],
    );
    let adapter_calls = AtomicUsize::new(0);

    let result = prepare_only_after_binding(&context, &registry, &work, &adapter_calls);

    assert!(
        matches!(
            &result,
            Err(NativeBindingError::TransportMismatch(
                NativeFacadeTransport::TypedAcp
            ))
        ),
        "TypedAcp binding returned {result:?} and reached the synthetic adapter {} time(s)",
        adapter_calls.load(Ordering::SeqCst)
    );
    assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);
}
