//! Which installed harnesses does the gate not admit from their rows' lists — and which of those
//! has the basic conformance battery admitted?
//!
//! ```sh
//! cargo run -q -p marion-testsupport --example harness_drift              # every unlisted release
//! cargo run -q -p marion-testsupport --example harness_drift -- --admitted # those a record admits
//! ```
//!
//! **stdout** is one `<program> <version>` line per harness whose installed release its row does
//! not list — with `--admitted`, only those this machine's conformance battery admitted for that
//! exact binary (`marion_testsupport::gate`, the gate's own verdict). The nightly canary
//! (`.github/workflows/canary.yml`) runs the battery on the first list and lists the second.
//! **stderr** is a table of every gated harness and what the gate made of it.
//!
//! It always exits 0: drift is the finding it reports, not a failure of the report.

use marion_testsupport::{Gate, gate, gated_rows};

fn main() {
    let admitted_only = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--admitted") => true,
        Some(other) => {
            eprintln!("usage: harness_drift [--admitted] (not {other:?})");
            std::process::exit(2);
        }
    };
    for spec in gated_rows() {
        let program = spec.program.expect("a gated row names its program");
        match gate(program) {
            Gate::Admitted {
                version,
                listed: true,
            } => eprintln!("{program:<9} {version:<10} listed"),
            Gate::Admitted { version, .. } => {
                eprintln!("{program:<9} {version:<10} admitted on its conformance record");
                println!("{program} {version}");
            }
            Gate::Refused {
                found: Some(version),
                diagnosis,
            } => {
                eprintln!("{program:<9} {version:<10} NOT ADMITTED: {diagnosis}");
                if !admitted_only {
                    println!("{program} {version}");
                }
            }
            Gate::Refused {
                found: None,
                diagnosis,
            } => eprintln!("{program:<9} {:<10} {diagnosis}", "-"),
            Gate::Absent => eprintln!("{program:<9} {:<10} not installed", "-"),
        }
    }
}
