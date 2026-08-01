//! `marion-supervisor` — the supervisor binary.
//!
//! `mcp` and `doctor` are **subcommands of this binary**, not separate ones (§10). The
//! user-facing `marion` command forwards `doctor` here, since the supervisor owns the registry
//! the check reads.

use std::io::{BufRead, Write};

mod bridge;

fn usage() -> ! {
    eprintln!("usage: marion-supervisor <mcp|doctor>");
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("mcp") => run_bridge(),
        Some("doctor") => {
            println!("marion doctor: no adapters registered yet (M1 in progress)");
        }
        _ => usage(),
    }
}

/// Serve MCP over stdio until the harness closes our stdin.
fn run_bridge() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(req) = bridge::parse(line) else { continue };
        let reply = match req {
            bridge::Request::Initialize { id } => Some(bridge::initialize_result(&id)),
            bridge::Request::ToolsList { id } => Some(bridge::tools_list_result(&id)),
            bridge::Request::ToolsCall { id, name, arguments } => {
                // M1 wires these to the registry next; for now the bridge is provably reachable,
                // which is what the S6 fixtures assert against.
                Some(bridge::tool_result(
                    &id,
                    &format!("marion received {name} with {arguments}"),
                    false,
                ))
            }
            bridge::Request::Notification => None,
            bridge::Request::Unknown { id, method } => Some(bridge::method_not_found(&id, &method)),
        };
        if let Some(r) = reply {
            let _ = writeln!(stdout, "{r}");
            let _ = stdout.flush();
        }
    }
}
