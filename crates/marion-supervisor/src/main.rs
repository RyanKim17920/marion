//! `marion-supervisor` — the supervisor binary.
//!
//! `mcp`, `serve` and `doctor` are **subcommands of this binary**, not separate ones (§10). The
//! user-facing `marion` command forwards `doctor` here, since the supervisor owns the registry
//! the check reads.
//!
//! `serve` is §10's M2+ row arriving: *"a detached `marion-supervisor`, which `marion` starts on
//! demand"*. It is on **this** binary and not on `marion` because §5.7 starts a supervisor *by a
//! client that finds nothing listening*, so no operator ever needs to type it — and because
//! `bin/marion.rs` refuses every argv[0] but `run`, a refusal that is pinned by a test and that
//! this change deliberately leaves standing. `detach.rs` owns all three of its stages.

use marion_supervisor::{detach, mcp};

fn usage() -> ! {
    eprintln!("usage: marion-supervisor <mcp|serve|doctor>");
    std::process::exit(2)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("mcp") => mcp::serve_stdio(),
        Some(detach::SERVE) => {
            let program = std::env::current_exe().unwrap_or_else(|_| "marion-supervisor".into());
            if let Err(e) = detach::run_serve(program, &argv[1..]) {
                // The one place a supervisor stage can speak. Stages 1 and 2 inherit the launcher's
                // stderr, so this reaches the operator who asked for a supervisor; stage 3's stderr
                // is a file beside the socket, so it reaches whoever goes looking afterwards.
                eprintln!("marion-supervisor: {e}");
                std::process::exit(1);
            }
        }
        Some("doctor") => {
            println!("marion doctor: no adapters registered yet (M1 in progress)");
        }
        _ => usage(),
    }
}
