//! `marion-canned` — run the canned provider as a process.
//!
//! ```sh
//! MARION_CANNED_PORT=8099 MARION_CANNED_REQLOG=/tmp/marion-requests.jsonl marion-canned
//! ```
//!
//! Point a harness's `base_url` at `http://127.0.0.1:<port>/v1` and the whole M1 hop runs with no
//! model, no key and no network.

use std::process::ExitCode;

use marion_provider::server::{CannedServer, Config};

fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("marion-canned: {e}");
            return ExitCode::FAILURE;
        }
    };
    let server = match CannedServer::start(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("marion-canned: cannot bind: {e}");
            return ExitCode::FAILURE;
        }
    };
    // stderr, not stdout: a caller may be capturing stdout as data.
    eprintln!(
        "marion-canned listening on {} (base_url {}) reqlog {}",
        server.addr(),
        server.base_url(),
        server.reqlog_path().display()
    );
    // The accept loop lives on its own thread; park this one so the process stays up.
    loop {
        std::thread::park();
    }
}
