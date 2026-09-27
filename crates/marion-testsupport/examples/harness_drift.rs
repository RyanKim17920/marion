//! Which installed harnesses has `PINNED_HARNESSES` not admitted?
//!
//! ```sh
//! cargo run -q -p marion-testsupport --example harness_drift
//! ```
//!
//! **stdout** is one `<program> <version>` line per harness whose installed release is not in its
//! `accepted` list — exactly the arguments `scripts/admit-harness.sh` takes, so the nightly canary
//! (`.github/workflows/canary.yml`) passes it straight through. **stderr** is a table of every
//! pinned harness: the installed version, or why there is none, and whether it is admitted.
//!
//! It always exits 0: drift is the finding it reports, not a failure of the report. A harness that
//! is absent or prints no version is on stderr only; it has nothing to admit.

use marion_testsupport::{PINNED_HARNESSES, installed_version};

fn main() {
    for pin in PINNED_HARNESSES {
        match installed_version(pin) {
            Ok(found) if pin.accepted.contains(&found.as_str()) => {
                eprintln!("{:<9} {found:<10} admitted", pin.program);
            }
            Ok(found) => {
                eprintln!(
                    "{:<9} {found:<10} NOT ADMITTED (accepted: {})",
                    pin.program,
                    pin.accepted.join(", ")
                );
                println!("{} {found}", pin.program);
            }
            Err(why) => eprintln!("{:<9} {:<10} {why}", pin.program, "-"),
        }
    }
}
