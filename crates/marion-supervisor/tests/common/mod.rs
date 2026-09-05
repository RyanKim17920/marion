//! **A real detached supervisor, and the declarations it writes** — the bed §11 item 28 step 5
//! made every child spawn need.
//!
//! Since step 5 a `spawn` is a socket call carrying `SpawnCaller { agent_id, node_token }`, checked
//! against the supervisor's own table. There is exactly one honest way for a test to hold those: be
//! — or drive — a node the supervisor really started. That takes a supervisor process and a node,
//! and this module is the two of them, shared by the files that need them rather than written twice.
//!
//! It is deliberately *not* in `marion-testsupport`: everything here is specific to this crate's
//! socket and its declaration formats, and a helper crate that reached back into the crate under
//! test would invert the dependency it exists to serve.

#![allow(dead_code)] // Each test binary uses the part of this bed it needs.

pub mod client;
pub mod run;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_proto::{Call, Frame, Outcome, Request, RequestId};
use marion_supervisor::socket::{SocketPaths, socket_paths};
use serde_json::Value;

unsafe extern "C" {
    fn getuid() -> u32;
}

/// A bound that exists only to fail: nothing here waits on work a test has not already caused.
const BOUND: Duration = Duration::from_secs(60);

/// A detached supervisor, started exactly as `detach.rs` stage 3 starts one.
///
/// **Spawned directly rather than through `detach::ensure_supervisor`**, for one reason: the
/// supervisor is the process that `exec`s a harness now, so a shim has to be on *its* `PATH`, and
/// `ensure_supervisor` spawns from the calling process's environment. That is `tasks/todo.md`'s
/// finding (d) — a node's environment is its supervisor's — met as a fixture constraint rather than
/// argued about. Stage 3's own entry check still passes: a `Command` child leads neither its session
/// nor its process group.
pub struct Supervisor {
    pub child: Child,
    pub paths: SocketPaths,
}

impl Supervisor {
    /// `state` and `key` are §2's pair — the state directory and the **canonical project root**
    /// (`socket::project_root`), which is what the socket path and the journal are both keyed on.
    pub fn start(
        state: &Path,
        key: &Path,
        path_env: &str,
        base_url: &str,
        idle_grace: Duration,
    ) -> Self {
        // SAFETY: reads the calling process's real uid and cannot fail.
        let paths = socket_paths(state, key, unsafe { getuid() });
        assert!(
            paths.overflow().is_none(),
            "this bed's socket must live under <state> beside its journal, and {:?} pushed it to \
             the /tmp fallback — a shared directory no fixture cleans",
            paths.socket()
        );
        let child = Command::new(env!("CARGO_BIN_EXE_marion-supervisor"))
            .args([
                "serve",
                "--state-dir",
                &state.to_string_lossy(),
                "--project-root",
                &key.to_string_lossy(),
                "--idle-grace-ms",
                &idle_grace.as_millis().to_string(),
                // Stated because `--auth canned` without an endpoint is refused by name
                // (`detached_spawn.rs`), and because a supervisor that guessed would point a real
                // credential somewhere marion did not choose.
                "--auth",
                "canned",
                "--base-url",
                base_url,
                "--detached",
            ])
            .env("PATH", path_env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the supervisor binary starts");
        let deadline = Instant::now() + BOUND;
        while std::os::unix::net::UnixStream::connect(paths.socket()).is_err() {
            assert!(
                Instant::now() < deadline,
                "the supervisor never bound {}; its stderr is inherited",
                paths.socket().display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Self { child, paths }
    }

    /// One request, one answer, on a connection of its own — the shape `courier.rs` uses.
    ///
    /// Notifications are skipped rather than read as answers: a connection can legitimately carry
    /// them, and a helper that took the next line would report one as the response.
    pub fn call(&self, call: Call) -> Result<Value, String> {
        let mut c = std::os::unix::net::UnixStream::connect(self.paths.socket())
            .expect("dial the supervisor");
        c.set_read_timeout(Some(BOUND)).unwrap();
        let frame = Frame::Request(Request::new(RequestId::Number(1), call));
        c.write_all(frame.to_line().as_bytes()).unwrap();
        c.flush().unwrap();
        let mut r = BufReader::new(c.try_clone().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                r.read_line(&mut line).expect("a frame arrives") > 0,
                "the supervisor closed the connection without answering"
            );
            if let Frame::Response(resp) = Frame::from_line(&line).expect("well-formed") {
                return match resp.outcome {
                    Outcome::Result(v) => Ok(v),
                    Outcome::Error(e) => Err(e.message),
                };
            }
        }
    }

    /// End it, whatever it is doing. Callers release their nodes first: the supervisor does not
    /// signal a node's process group when it dies, so killing it first strands every blocked child.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// **The `env` block marion wrote into a node's own declaration**, read back off disk.
///
/// This is a fixture reading marion's homework, and it is deliberate: a hand-written environment
/// block would pass whatever marion stopped writing. A bridge started from *this* fails every test
/// that uses it if `bridge_env_pairs` ever drops `MARION_NODE_TOKEN`.
///
/// The declaration is whichever document that node's adapter generated — codex's `config.toml`, a
/// claude `.mcp.json`, gemini's or opencode's JSON — so the scan below accepts both spellings a
/// key/value pair takes across them (`K = "v"` and `"K": "v"`) and nothing else. Parsed by hand
/// rather than with a TOML *and* a JSON dependency, and narrowly: only keys marion itself defines,
/// only double-quoted values.
pub fn declaration_of(state: &Path, agent_id: &AgentId) -> BTreeMap<String, String> {
    let mut candidates = Vec::new();
    walk(state, &mut |p| {
        if p.to_string_lossy().contains(&agent_id.0) {
            candidates.push(p.to_path_buf());
        }
    });
    let mut out = BTreeMap::new();
    let mut read_any = false;
    for path in &candidates {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if !text.contains("MARION_NODE_TOKEN") {
            continue;
        }
        read_any = true;
        for (at, _) in text.match_indices("MARION_") {
            let rest = &text[at..];
            let key_len = rest
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(rest.len());
            let (key, rest) = rest.split_at(key_len);
            // `"KEY": "v"` and `KEY = "v"`, and nothing else: a mention in prose has neither.
            let rest = rest.trim_start().trim_start_matches('"').trim_start();
            let Some(rest) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('=')) else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some(end) = rest.find('"') else { continue };
            out.insert(key.to_string(), rest[..end].to_string());
        }
    }
    assert!(
        read_any,
        "marion wrote no declaration carrying a capability token for node {} under {}; without one \
         no bridge could ever spawn (§5.4)",
        agent_id.0,
        state.display()
    );
    assert!(
        out.contains_key("MARION_NODE_TOKEN"),
        "the declaration marion wrote for {} names a token and this could not read it back",
        agent_id.0
    );
    out
}

/// Every file under `dir`, depth first. A walk rather than a computed path: the project hash that
/// names the directory is derived inside marion, and a test that recomputed it would be asserting
/// against its own copy of the derivation.
pub fn walk(dir: &Path, f: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, f);
        } else {
            f(&p);
        }
    }
}
