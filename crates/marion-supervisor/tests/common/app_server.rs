//! **A `codex` that speaks app-server and does what a test's shell says** — for the files whose
//! bed replaces the real binary with a shim.
//!
//! codex's headless row is `codex app-server` (S36), so a shim that only printed `exec` frames and
//! exited is a node that never answers `initialize`. This one is the protocol's minimum, in the
//! S36 shapes: it answers the handshake and `thread/start`/`thread/resume` (naming the thread, or
//! the one resumed), reports marion's MCP server ready, and runs each turn by handing the turn's
//! text to the test's own shell hook as `$1`. Every line the hook prints is forwarded as a frame
//! (app-server notifications, which the test writes in S36's shapes), and the turn completes when
//! the hook exits — `failed` if it exits non-zero; in a turn the hook sees `MARION_SHIM_TURN=1` and the server's own argv (marion's
//! `-c` pairs) as `MARION_SHIM_ARGV`. Any invocation that is not `app-server` — `--version`, `login
//! status` — is the hook's outright, with its argv, so a bed can make the probe hang or answer a
//! status probe. stdin EOF ends the server, as P9 measured the real one does.

use std::path::{Path, PathBuf};

/// The server half, in Python so it can read frames while it waits on nothing else.
const SERVER: &str = r#"#!/usr/bin/env python3
import json, os, subprocess, sys, threading
hook = os.path.abspath(__file__) + "-hook.sh"
if sys.argv[1:2] != ["app-server"]:
    # `--version`, `login status`, anything that is not the server: the hook's, with its argv.
    os.execv("/bin/sh", ["/bin/sh", hook] + sys.argv[1:])
os.environ["MARION_SHIM_ARGV"] = " ".join(sys.argv[1:])
# One writer at a time: a turn's frames come from its own thread while this one answers requests.
lock = threading.Lock()
def write(line):
    with lock:
        sys.stdout.write(line + "\n"); sys.stdout.flush()
def out(o):
    write(json.dumps(o))
thread = "shim-thread-%d" % os.getpid()
turn = 0
# **A turn runs beside the reader, as in the real server**: a `turn/steer` sent while the hook
# blocks is answered then, not after the turn, so a folded message is delivered mid-turn rather
# than left unanswered in marion's driver. Turns stay one at a time: a `turn/start` waits for the
# previous turn, and stdin EOF lets the running one finish before exit. A daemon thread, so a signal
# that ends the reader (SIGINT's KeyboardInterrupt) ends the server as it did when the turn ran here.
running = None
def run_turn(u, text):
    p = subprocess.Popen(["/bin/sh", hook, text], stdout=subprocess.PIPE, text=True,
                         env=dict(os.environ, MARION_SHIM_TURN="1"))
    for frame in p.stdout:
        frame = frame.strip()
        if frame:
            write(frame)
    failed = p.wait() != 0
    out({"method": "turn/completed", "params": {"threadId": thread, "turn": {"id": u, "status": "failed" if failed else "completed", "items": [], "error": {"message": "the shim's hook exited non-zero"} if failed else None}}})
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    meth = m.get("method"); i = m.get("id")
    if meth == "initialize":
        out({"id": i, "result": {"userAgent": "codex-shim"}})
    elif meth in ("thread/start", "thread/resume"):
        thread = m.get("params", {}).get("threadId", thread)
        out({"id": i, "result": {"thread": {"id": thread}}})
        out({"method": "mcpServer/startupStatus/updated", "params": {"threadId": thread, "name": "marion", "status": "ready", "error": None}})
    elif meth == "turn/start":
        if running is not None:
            running.join()
        turn += 1; u = "shim-turn-%d" % turn
        text = "".join(p.get("text", "") for p in m.get("params", {}).get("input", []))
        out({"id": i, "result": {"turn": {"id": u, "status": "inProgress"}}})
        out({"method": "turn/started", "params": {"threadId": thread, "turn": {"id": u}}})
        running = threading.Thread(target=run_turn, args=(u, text), daemon=True)
        running.start()
    elif meth == "turn/steer":
        out({"id": i, "result": {"turnId": m["params"].get("expectedTurnId")}})
    elif meth == "turn/interrupt":
        out({"id": i, "result": {}})
    elif i is not None and meth is not None:
        out({"id": i, "error": {"code": -32601, "message": "the shim does not serve " + meth}})
if running is not None:
    running.join()
"#;

/// Write the shim as `codex` in `dir`, running `hook` (a `/bin/sh` body) for every turn — the
/// turn's text as `$1` — and for every invocation that is not `app-server`, with its argv.
/// Returns the binary's path.
pub fn fake_codex(dir: &Path, hook: &str) -> PathBuf {
    fake_codex_as(dir, "codex", hook)
}

/// [`fake_codex`] under another name, for a bed whose `codex` routes some nodes to the real binary
/// and the rest to this ([`real_codex_at_depths`]).
pub fn fake_codex_as(dir: &Path, name: &str, hook: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).expect("shim dir");
    let bin = dir.join(name);
    std::fs::write(&bin, SERVER).expect("write the shim");
    std::fs::write(dir.join(format!("{name}-hook.sh")), hook).expect("write the shim's hook");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    bin
}

/// **A `codex` in `dir` that is the real binary for the nodes whose depth is in `depths` and the
/// app-server shim for every other**, answering `--version` with `version` itself.
///
/// The server learns what a node is for only from its first turn, too late to choose which binary
/// serves it, so the choice is made from what marion wrote before the launch: a canned node's
/// `$CODEX_HOME/config.toml` names its depth in marion's bridge declaration (`MARION_DEPTH`).
/// `hook` is the shim's, as for [`fake_codex`].
pub fn real_codex_at_depths(
    dir: &Path,
    real: &Path,
    depths: std::ops::RangeInclusive<u32>,
    version: &str,
    hook: &str,
) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fake = fake_codex_as(dir, "codex-shim", hook);
    let bin = dir.join("codex");
    let router = format!(
        "#!/bin/sh\n\
         case \"$1\" in --version) echo {version}; exit 0 ;; esac\n\
         d=$(sed -n 's/.*MARION_DEPTH = \"\\([0-9]*\\)\".*/\\1/p' \
           \"${{CODEX_HOME:-/nonexistent}}/config.toml\" 2>/dev/null)\n\
         if [ \"$1\" = app-server ] && [ -n \"$d\" ] && [ \"$d\" -ge {lo} ] && [ \"$d\" -le {hi} ]; then\n\
           exec {real} \"$@\"\n\
         fi\n\
         exec {fake} \"$@\"\n",
        version = super::shell_quote(Path::new(version)),
        lo = depths.start(),
        hi = depths.end(),
        real = super::shell_quote(real),
        fake = super::shell_quote(&fake),
    );
    std::fs::write(&bin, router).expect("write the router");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    bin
}
