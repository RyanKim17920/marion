//! Native launch state reaches the shipped supervisor socket and stops at the root boundary.
//!
//! The two marker programs make both possible downgrade directions observable: executing the
//! context's own `program` writes one marker, while falling back to the managed Claude adapter
//! writes another. The journal and agent directory independently expose node creation. Every
//! refusal below checks all four after the response that caused it.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use marion_proto::params::AgentSpawnParams;
use marion_proto::{
    Call, FailureKind, Frame, NativeEnvVarV1, NativeLaunchContextV1, OpaqueOsValueV1, Outcome,
    Request, RequestId, RpcError, SpawnCaller, TerminalGeometryV1,
};
use marion_testsupport::{Scratch, fixture_repo, scratch};

mod common;
use common::Supervisor;

const BOUND: Duration = Duration::from_secs(30);

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("the marker program is written");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("the marker program is executable");
}

fn opaque(value: &OsStr) -> OpaqueOsValueV1 {
    OpaqueOsValueV1::from_os_str(value).expect("Unix preserves native launch bytes")
}

struct Bed {
    _scratch: Scratch,
    repo: PathBuf,
    state: PathBuf,
    native_program: PathBuf,
    native_marker: PathBuf,
    managed_marker: PathBuf,
    supervisor: Supervisor,
}

impl Bed {
    fn new() -> Self {
        let scratch = scratch("native-facade-root-gate");
        let repo = fixture_repo(&scratch);
        let state = scratch.join("state");
        let bin = scratch.join("bin");
        std::fs::create_dir_all(&bin).expect("the shim directory exists");

        let native_marker = scratch.join("native-invoked");
        let managed_marker = scratch.join("managed-invoked");
        let native_program = bin.join("native-program");
        executable(
            &native_program,
            &format!("#!/bin/sh\n: > {}\nexit 0\n", shell_quote(&native_marker)),
        );
        executable(
            &bin.join("claude"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.1.222'; exit 0; fi\n\
                 : > {}\nexit 0\n",
                shell_quote(&managed_marker)
            ),
        );

        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let path = format!("{}:{}", bin.display(), inherited_path.to_string_lossy());
        let key = marion_supervisor::socket::project_root(&repo);
        let supervisor = Supervisor::start(
            &state,
            &key,
            &path,
            "http://127.0.0.1:8099/v1",
            Duration::from_secs(600),
        );

        Self {
            _scratch: scratch,
            repo,
            state,
            native_program,
            native_marker,
            managed_marker,
            supervisor,
        }
    }

    fn context(&self) -> NativeLaunchContextV1 {
        NativeLaunchContextV1::new(
            opaque(self.native_program.as_os_str()),
            vec![opaque(OsStr::new("--native-root"))],
            opaque(self.repo.as_os_str()),
            vec![NativeEnvVarV1 {
                name: opaque(OsStr::new("MARION_NATIVE_GATE_PROBE")),
                value: opaque(OsStr::new("present")),
            }],
            TerminalGeometryV1 {
                cols: 101,
                rows: 37,
                xpixel: 0,
                ypixel: 0,
            },
        )
    }

    fn root_params(&self) -> AgentSpawnParams {
        AgentSpawnParams {
            agent_type: "claude".into(),
            prompt: "unused because native launch is gated".into(),
            native_launch: Some(self.context()),
            caller: None,
            repo: Some(self.repo.clone()),
            acceptance_criteria: vec![],
            writable_scope: vec![],
            timeout_secs: Some(1),
            model: None,
            no_change_record: None,
            pane: None,
            isolation: None,
            allow_concurrent_writes: None,
        }
    }

    fn send_call(&self, params: AgentSpawnParams) -> Result<serde_json::Value, RpcError> {
        let frame = Frame::Request(Request::new(RequestId::Number(1), Call::AgentSpawn(params)));
        self.send_line(&frame.to_line())
    }

    fn send_line(&self, line: &str) -> Result<serde_json::Value, RpcError> {
        let mut socket = UnixStream::connect(self.supervisor.paths.socket())
            .expect("the shipped supervisor is listening");
        socket.set_read_timeout(Some(BOUND)).unwrap();
        socket
            .write_all(line.as_bytes())
            .expect("the request writes");
        if !line.ends_with('\n') {
            socket.write_all(b"\n").expect("the request terminates");
        }
        socket.flush().expect("the request flushes");

        let mut reader = BufReader::new(socket);
        loop {
            let mut answer = String::new();
            assert!(
                reader.read_line(&mut answer).expect("a response arrives") > 0,
                "the supervisor closed without answering"
            );
            let Frame::Response(response) = Frame::from_line(&answer).expect("a valid response")
            else {
                continue;
            };
            return match response.outcome {
                Outcome::Error(error) => Err(error),
                Outcome::Result(value) => Ok(value),
            };
        }
    }

    fn assert_zero_artifacts(&self, case: &str) {
        let project = marion_core::paths::ProjectDir::new(
            &self.state,
            &marion_supervisor::socket::project_root(&self.repo),
        );
        let journal = std::fs::read(project.journal()).unwrap_or_default();
        assert!(
            journal.is_empty(),
            "{case}: the refusal wrote to the journal"
        );
        let agent_dirs = std::fs::read_dir(project.agents_dir())
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0);
        assert_eq!(
            agent_dirs, 0,
            "{case}: the refusal created a node directory"
        );
        assert!(
            !self.native_marker.exists(),
            "{case}: the native context program was invoked"
        );
        assert!(
            !self.managed_marker.exists(),
            "{case}: native launch silently fell back to the managed adapter"
        );
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        self.supervisor.stop();
    }
}

#[test]
fn native_launch_frames_are_refused_at_the_real_socket_before_every_launch_artifact() {
    let bed = Bed::new();

    let v1 = bed.send_call(bed.root_params());
    bed.assert_zero_artifacts("valid V1");
    let v1 = v1.expect_err("valid V1 must be refused");
    assert_eq!(v1.code, FailureKind::Refused.code(), "V1 refusal code");
    assert_eq!(v1.kind(), Some(FailureKind::Refused), "V1 refusal kind");
    assert!(
        v1.message.contains("native facade transport is not ready"),
        "V1 names transport readiness: {}",
        v1.message
    );
    let v1_frame = Frame::Request(Request::new(
        RequestId::Number(2),
        Call::AgentSpawn(bed.root_params()),
    ));
    let mut v2: serde_json::Value =
        serde_json::from_str(v1_frame.to_line().trim()).expect("the V1 frame is JSON");
    v2["params"]["native_launch"]["wire_version"] = serde_json::json!(2);
    let v2 = bed.send_line(&serde_json::to_string(&v2).expect("the V2 frame serializes"));
    bed.assert_zero_artifacts("raw V2");
    let v2 = v2.expect_err("raw V2 must fail deserialization");
    assert_eq!(v2.code, marion_proto::error::INVALID_PARAMS, "V2 code");
    assert_eq!(v2.kind(), None, "V2 fails before a Call exists");
    assert!(
        v2.message.contains("wire_version 2") && v2.message.contains("expected 1"),
        "V2 names the safe protocol-version error: {}",
        v2.message
    );
    let mut child = bed.root_params();
    child.caller = Some(SpawnCaller {
        agent_id: marion_core::contract::AgentId("0199c0ff-ee00-7000-8000-000000000001".into()),
        node_token: "not-a-token".into(),
    });
    child.repo = None;
    let child = bed.send_call(child);
    bed.assert_zero_artifacts("child misuse");
    let child = child.expect_err("child native state must be refused");
    assert_eq!(child.code, FailureKind::Refused.code(), "child code");
    assert!(
        child
            .message
            .contains("native launch context belongs only on a root"),
        "child misuse is distinct from readiness and token lookup: {}",
        child.message
    );
    let mut legacy = bed.root_params();
    legacy.native_launch = None;
    legacy.agent_type = "no-such-agent-type".into();
    let legacy = bed.send_call(legacy);
    bed.assert_zero_artifacts("legacy None");
    let legacy = legacy.expect_err("the fixture's legacy agent type does not exist");
    assert_eq!(legacy.code, FailureKind::Refused.code(), "legacy code");
    assert!(
        legacy
            .message
            .contains("the agent type is not one this build has"),
        "legacy `native_launch: None` retains its prior root path: {}",
        legacy.message
    );
}
