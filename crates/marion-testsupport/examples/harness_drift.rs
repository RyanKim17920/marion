//! Which installed harnesses does `PINNED_HARNESSES` not list — and which of those has the basic
//! conformance battery admitted?
//!
//! ```sh
//! cargo run -q -p marion-testsupport --example harness_drift              # every unlisted release
//! cargo run -q -p marion-testsupport --example harness_drift -- --admitted # those a record admits
//! ```
//!
//! **stdout** is one `<program> <version>` line per harness whose installed release is not in its
//! `accepted` list — with `--admitted`, only those whose conformance record on this machine admits
//! that exact binary (`marion_testsupport::admitted_by_record`, the gate's own verdict). The
//! nightly canary (`.github/workflows/canary.yml`) runs the battery on the first list and admits
//! the second. **stderr** is a table of every pinned harness: the installed version, or why there
//! is none, whether it is listed, and for one that is not, what its record says.
//!
//! It always exits 0: drift is the finding it reports, not a failure of the report. A harness that
//! is absent or prints no version is on stderr only; it has nothing to admit.

use marion_testsupport::{PINNED_HARNESSES, ReleaseStores, admitted_by_record, installed_version};

fn main() {
    let admitted_only = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--admitted") => true,
        Some(other) => {
            eprintln!("usage: harness_drift [--admitted] (not {other:?})");
            std::process::exit(2);
        }
    };
    let state = ReleaseStores::from_env().state;
    let path = std::env::var_os("PATH").unwrap_or_default();
    for pin in PINNED_HARNESSES {
        match installed_version(pin) {
            Ok(found) if pin.accepted.contains(&found.as_str()) => {
                eprintln!("{:<9} {found:<10} listed", pin.program);
            }
            Ok(found) => {
                let record = admitted_by_record(pin, &found, &path, &state);
                eprintln!(
                    "{:<9} {found:<10} NOT LISTED (accepted: {}); {}",
                    pin.program,
                    pin.accepted.join(", "),
                    record.as_ref().map_or_else(
                        |why| why.clone(),
                        |()| "admitted on its conformance record".to_string()
                    )
                );
                if !admitted_only || record.is_ok() {
                    println!("{} {found}", pin.program);
                }
            }
            Err(why) => eprintln!("{:<9} {:<10} {why}", pin.program, "-"),
        }
    }
}
