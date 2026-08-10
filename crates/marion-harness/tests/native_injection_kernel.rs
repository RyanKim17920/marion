use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use marion_harness::mcp_bridge::MarionMcpBridge;
use marion_harness::{
    NativeDocument, NativeEnvironmentView, NativeInjection, NativeInjectionAdapter,
    NativeInjectionError, NativeNodeContext, NativeProcessBase, NativeTerminalGeometry,
    assemble_native, validate_native_process_values,
};

const GEOMETRY: NativeTerminalGeometry = NativeTerminalGeometry {
    cols: 132,
    rows: 43,
    xpixel: 7,
    ypixel: 11,
};

fn base_with(
    program: impl Into<OsString>,
    user_argv: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: impl Into<PathBuf>,
) -> NativeProcessBase {
    NativeProcessBase {
        program: program.into(),
        user_argv,
        env,
        cwd: cwd.into(),
        geometry: GEOMETRY,
    }
}

fn empty_injection() -> NativeInjection {
    NativeInjection {
        argv_prefix: vec![],
        env_overlay: vec![],
        documents: vec![],
    }
}

struct PrefixAdapter;

impl NativeInjectionAdapter for PrefixAdapter {
    fn prepare_native(
        &self,
        context: &NativeNodeContext<'_>,
    ) -> Result<NativeInjection, NativeInjectionError> {
        let replacement = context
            .environment
            .get(OsStr::new("REPLACE"))
            .ok_or_else(|| NativeInjectionError::Adapter("missing REPLACE".into()))?;
        if context.allowed_marion_tools != ["spawn_agent", "send_message"] {
            return Err(NativeInjectionError::Adapter(
                "unexpected Marion tool policy".into(),
            ));
        }
        if context.bridge.args != [OsString::from("mcp")] {
            return Err(NativeInjectionError::Adapter(
                "unexpected MCP bridge declaration".into(),
            ));
        }

        Ok(NativeInjection {
            argv_prefix: vec![OsString::from("--marion-prefix")],
            env_overlay: vec![(OsString::from("REPLACE"), replacement.to_owned())],
            documents: vec![NativeDocument {
                path: context.document_dir.join("adapter.json"),
                contents: b"{\"adapter\":\"synthetic\"}".to_vec(),
            }],
        })
    }
}

#[test]
fn adapter_prefix_precedes_boundary_tail_and_documents_remain_a_pure_handoff() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock is after the Unix epoch")
        .as_nanos();
    let document_dir = std::env::temp_dir().join(format!(
        "marion-native-kernel-{}-{nonce}",
        std::process::id()
    ));
    assert!(!document_dir.exists());

    let base = base_with(
        "/opt/bin/atlas",
        vec![OsString::from("--"), OsString::from("--help")],
        vec![(OsString::from("REPLACE"), OsString::from("old"))],
        "/work/project",
    );
    let bridge = MarionMcpBridge {
        program: OsString::from("/opt/bin/marion-supervisor"),
        args: vec![OsString::from("mcp")],
        env: vec![(OsString::from("MARION_AGENT_ID"), OsString::from("root"))],
    };
    let allowed_marion_tools = ["spawn_agent", "send_message"];
    let injection = PrefixAdapter
        .prepare_native(&NativeNodeContext {
            bridge: &bridge,
            document_dir: &document_dir,
            allowed_marion_tools: &allowed_marion_tools,
            environment: NativeEnvironmentView::validate(&base.env)
                .expect("the adapter sees a validated read-only environment"),
        })
        .expect("the synthetic adapter prepares its injection");

    let prepared = assemble_native(base, injection).expect("the launch is assembled");

    assert_eq!(
        prepared.invocation.args,
        ["--marion-prefix", "--", "--help"].map(OsString::from),
    );
    assert!(prepared.invocation.requires_env_clear());
    assert_eq!(
        prepared.invocation.program,
        OsString::from("/opt/bin/atlas")
    );
    assert_eq!(prepared.invocation.cwd, Path::new("/work/project"));
    assert_eq!(prepared.invocation.geometry, GEOMETRY);
    assert_eq!(
        prepared.documents[0].path,
        document_dir.join("adapter.json")
    );
    assert_eq!(
        prepared.documents[0].contents,
        b"{\"adapter\":\"synthetic\"}"
    );
    assert!(!document_dir.exists());
}

#[test]
fn prefix_is_first_for_empty_and_flag_shaped_user_tails() {
    let cases = [
        (vec![], vec!["prefix"]),
        (vec![""], vec!["prefix", ""]),
        (vec!["--"], vec!["prefix", "--"]),
        (vec!["--help"], vec!["prefix", "--help"]),
        (vec!["--version"], vec!["prefix", "--version"]),
        (
            vec!["--", "--help", "", "--version", "tail"],
            vec!["prefix", "--", "--help", "", "--version", "tail"],
        ),
    ];

    for (tail, expected) in cases {
        let prepared = assemble_native(
            base_with(
                "program",
                tail.into_iter().map(OsString::from).collect(),
                vec![],
                "/cwd",
            ),
            NativeInjection {
                argv_prefix: vec![OsString::from("prefix")],
                env_overlay: vec![],
                documents: vec![],
            },
        )
        .expect("flag-shaped user values stay opaque");

        assert_eq!(
            prepared.invocation.args,
            expected.into_iter().map(OsString::from).collect::<Vec<_>>()
        );
    }
}

#[test]
fn overlay_replaces_in_place_appends_in_adapter_order_and_preserves_untouched_slots() {
    let prepared = assemble_native(
        base_with(
            "program",
            vec![],
            vec![
                (OsString::from("FIRST"), OsString::from("one")),
                (OsString::from("REPLACE"), OsString::from("old")),
                (OsString::from("MIDDLE"), OsString::from("three")),
                (OsString::from("LAST"), OsString::from("four")),
            ],
            "/cwd",
        ),
        NativeInjection {
            argv_prefix: vec![],
            env_overlay: vec![
                (OsString::from("REPLACE"), OsString::from("new")),
                (OsString::from("APPEND_B"), OsString::from("six")),
                (OsString::from("APPEND_A"), OsString::from("five")),
            ],
            documents: vec![],
        },
    )
    .expect("the ordered environment merges");

    assert_eq!(
        prepared.invocation.env,
        [
            ("FIRST", "one"),
            ("REPLACE", "new"),
            ("MIDDLE", "three"),
            ("LAST", "four"),
            ("APPEND_B", "six"),
            ("APPEND_A", "five"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    );
}

#[test]
fn every_base_marion_identity_is_removed_but_similar_names_survive() {
    let prepared = assemble_native(
        base_with(
            "program",
            vec![],
            vec![
                (OsString::from("KEEP"), OsString::from("one")),
                (OsString::from("MARION_AGENT_ID"), OsString::from("forged")),
                (
                    OsString::from("MARION_NODE_TOKEN"),
                    OsString::from("forged"),
                ),
                (
                    OsString::from("MARION_FUTURE_IDENTITY"),
                    OsString::from("forged"),
                ),
                (OsString::from("marion_lower"), OsString::from("kept")),
                (OsString::from("MARION"), OsString::from("kept")),
            ],
            "/cwd",
        ),
        empty_injection(),
    )
    .expect("reserved base identities are filtered");

    assert_eq!(
        prepared.invocation.env,
        [
            (OsString::from("KEEP"), OsString::from("one")),
            (OsString::from("marion_lower"), OsString::from("kept")),
            (OsString::from("MARION"), OsString::from("kept")),
        ]
    );
}

#[test]
fn adapter_cannot_overlay_a_reserved_marion_identity() {
    let error = assemble_native(
        base_with("program", vec![], vec![], "/cwd"),
        NativeInjection {
            argv_prefix: vec![],
            env_overlay: vec![(
                OsString::from("MARION_FUTURE_IDENTITY"),
                OsString::from("forged"),
            )],
            documents: vec![],
        },
    )
    .expect_err("an adapter cannot forge Marion identity");

    assert!(matches!(
        error,
        NativeInjectionError::ReservedMarionEnvironment
    ));
}

#[test]
fn base_environment_name_failures_remain_distinct() {
    let empty = NativeEnvironmentView::validate(&[(OsString::new(), OsString::from("value"))])
        .expect_err("empty base names are refused");
    assert!(matches!(empty, NativeInjectionError::EmptyEnvironmentName));

    let equals =
        NativeEnvironmentView::validate(&[(OsString::from("BAD=NAME"), OsString::from("value"))])
            .expect_err("equals signs are refused in base names");
    assert!(matches!(
        equals,
        NativeInjectionError::InvalidEnvironmentName
    ));

    let duplicate = NativeEnvironmentView::validate(&[
        (OsString::from("DUP"), OsString::from("one")),
        (OsString::from("DUP"), OsString::from("two")),
    ])
    .expect_err("duplicate base names are refused");
    assert!(matches!(
        duplicate,
        NativeInjectionError::DuplicateEnvironmentName
    ));
}

#[test]
fn duplicate_overlay_names_have_their_own_failure() {
    let error = assemble_native(
        base_with("program", vec![], vec![], "/cwd"),
        NativeInjection {
            argv_prefix: vec![],
            env_overlay: vec![
                (OsString::from("DUP"), OsString::from("one")),
                (OsString::from("DUP"), OsString::from("two")),
            ],
            documents: vec![],
        },
    )
    .expect_err("duplicate overlay names are refused");

    assert!(matches!(error, NativeInjectionError::DuplicateOverlayName));
}

#[test]
fn nul_is_refused_in_every_process_value_category() {
    let base_cases = [
        base_with("pro\0gram", vec![], vec![], "/cwd"),
        base_with(
            "program",
            vec![OsString::from("first"), OsString::from("ta\0il")],
            vec![],
            "/cwd",
        ),
        base_with(
            "program",
            vec![],
            vec![(OsString::from("NA\0ME"), OsString::from("value"))],
            "/cwd",
        ),
        base_with(
            "program",
            vec![],
            vec![(OsString::from("NAME"), OsString::from("val\0ue"))],
            "/cwd",
        ),
        base_with("program", vec![], vec![], PathBuf::from("/cw\0d")),
    ];

    for base in base_cases {
        assert!(matches!(
            assemble_native(base, empty_injection()),
            Err(NativeInjectionError::EmbeddedNul)
        ));
    }

    let prefix_error = assemble_native(
        base_with("program", vec![], vec![], "/cwd"),
        NativeInjection {
            argv_prefix: vec![OsString::from("pre\0fix")],
            env_overlay: vec![],
            documents: vec![],
        },
    )
    .expect_err("NUL in an injection prefix is refused");
    assert!(matches!(prefix_error, NativeInjectionError::EmbeddedNul));

    let overlay_name_error = assemble_native(
        base_with("program", vec![], vec![], "/cwd"),
        NativeInjection {
            argv_prefix: vec![],
            env_overlay: vec![(OsString::from("NA\0ME"), OsString::from("value"))],
            documents: vec![],
        },
    )
    .expect_err("NUL in an overlay name is refused");
    assert!(matches!(
        overlay_name_error,
        NativeInjectionError::EmbeddedNul
    ));

    let overlay_value_error = assemble_native(
        base_with("program", vec![], vec![], "/cwd"),
        NativeInjection {
            argv_prefix: vec![],
            env_overlay: vec![(OsString::from("NAME"), OsString::from("val\0ue"))],
            documents: vec![],
        },
    )
    .expect_err("NUL in an overlay value is refused");
    assert!(matches!(
        overlay_value_error,
        NativeInjectionError::EmbeddedNul
    ));
}

#[test]
fn exact_environment_lookup_borrows_the_original_value() {
    let entries = [
        (OsString::from("NAME"), OsString::from("first")),
        (OsString::from("Name"), OsString::from("second")),
    ];
    let view = NativeEnvironmentView::validate(&entries).expect("the environment is valid");

    let found = view
        .get(OsStr::new("Name"))
        .expect("lookup compares exact OS strings");
    assert_eq!(found, OsStr::new("second"));
    assert!(std::ptr::eq(found, entries[1].1.as_os_str()));
    assert!(view.get(OsStr::new("NAME ")).is_none());
}

#[test]
fn shared_validator_accepts_opaque_but_complete_process_state() {
    validate_native_process_values(
        OsStr::new("program"),
        &[OsString::from("--"), OsString::from("--help")],
        &[(OsString::from("PATH"), OsString::from("/bin"))],
        Path::new("/cwd"),
    )
    .expect("the complete process state is valid");
}

#[cfg(unix)]
#[test]
fn invalid_unix_bytes_survive_every_opaque_slot_and_order_boundary() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let program = OsString::from_vec(vec![b'p', b'r', b'o', 0xff]);
    let tails = vec![
        OsString::from_vec(vec![b'a', 0xfe]),
        OsString::from("--"),
        OsString::from_vec(vec![b'b', 0xfd]),
    ];
    let cwd = PathBuf::from(OsString::from_vec(vec![b'/', b'c', b'w', b'd', 0xfc]));
    let prepared = assemble_native(
        base_with(
            program.clone(),
            tails.clone(),
            vec![
                (
                    OsString::from("BEFORE"),
                    OsString::from_vec(vec![b'o', 0xfb]),
                ),
                (
                    OsString::from_vec(vec![b'O', b'P', 0xfa]),
                    OsString::from_vec(vec![b'v', 0xf9]),
                ),
                (
                    OsString::from("REPLACE"),
                    OsString::from_vec(vec![b'o', b'l', b'd', 0xf8]),
                ),
                (
                    OsString::from("AFTER"),
                    OsString::from_vec(vec![b'z', 0xf7]),
                ),
            ],
            cwd.clone(),
        ),
        NativeInjection {
            argv_prefix: vec![OsString::from_vec(vec![b'p', b'r', b'e', 0xf6])],
            env_overlay: vec![
                (
                    OsString::from("REPLACE"),
                    OsString::from_vec(vec![b'n', b'e', b'w', 0xf5]),
                ),
                (
                    OsString::from_vec(vec![b'A', b'P', b'P', 0xf4]),
                    OsString::from_vec(vec![b'v', 0xf3]),
                ),
            ],
            documents: vec![],
        },
    )
    .expect("invalid UTF-8 remains opaque on Unix");

    assert_eq!(prepared.invocation.program.as_bytes(), program.as_bytes());
    assert_eq!(prepared.invocation.args[0].as_bytes(), b"pre\xf6");
    assert_eq!(prepared.invocation.args[1].as_bytes(), tails[0].as_bytes());
    assert_eq!(prepared.invocation.args[2].as_bytes(), b"--");
    assert_eq!(prepared.invocation.args[3].as_bytes(), tails[2].as_bytes());
    assert_eq!(
        prepared.invocation.cwd.as_os_str().as_bytes(),
        cwd.as_os_str().as_bytes()
    );
    assert_eq!(prepared.invocation.env[0].0.as_bytes(), b"BEFORE");
    assert_eq!(prepared.invocation.env[0].1.as_bytes(), b"o\xfb");
    assert_eq!(prepared.invocation.env[1].0.as_bytes(), b"OP\xfa");
    assert_eq!(prepared.invocation.env[1].1.as_bytes(), b"v\xf9");
    assert_eq!(prepared.invocation.env[2].0.as_bytes(), b"REPLACE");
    assert_eq!(prepared.invocation.env[2].1.as_bytes(), b"new\xf5");
    assert_eq!(prepared.invocation.env[3].0.as_bytes(), b"AFTER");
    assert_eq!(prepared.invocation.env[3].1.as_bytes(), b"z\xf7");
    assert_eq!(prepared.invocation.env[4].0.as_bytes(), b"APP\xf4");
    assert_eq!(prepared.invocation.env[4].1.as_bytes(), b"v\xf3");
}
