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

use marion_supervisor::{detach, doctor, mcp, preflight};

fn usage() -> ! {
    eprintln!(
        "usage: marion-supervisor doctor [--adapter] [--harness <name>] …\n\
         \x20      marion-supervisor --version\n\
         \n\
         `marion-supervisor doctor` is the same as `marion doctor`. `mcp` and `serve` are \
         internal:\nmarion starts them itself, and they are not commands to type."
    );
    std::process::exit(2)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("--version" | "-V") => println!("marion-supervisor {}", env!("CARGO_PKG_VERSION")),
        Some("mcp") => mcp::serve_stdio(mcp::Principal::Node),
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
        Some("doctor") if marion_supervisor::provider_check::parse_args(&argv[1..]).is_some() => {
            let parsed = marion_supervisor::provider_check::parse_args(&argv[1..])
                .expect("checked by the guard");
            match parsed
                .and_then(|model| marion_supervisor::provider_check::report(model.as_deref()))
            {
                Ok(text) => print!("{text}"),
                Err(e) => {
                    eprintln!("marion doctor --providers: {e}");
                    std::process::exit(2);
                }
            }
        }
        Some("doctor") => match doctor::parse_args(&argv[1..]) {
            Ok(opts) => {
                let rows = doctor::run(&opts);
                let environment = preflight::checks(&preflight::gather());
                print!("{}", doctor::render(&rows, &environment));
                // Bare `marion doctor` also checks the provider keys `marion login` stored — the
                // `--providers` report, one `GET …/models` per credential, which spends nothing. A
                // store it cannot read is said, never a failure of the machine's own checks.
                if argv.len() == 1 {
                    match marion_supervisor::provider_check::report(None) {
                        Ok(text) => print!("\n{text}"),
                        Err(e) => println!("\nproviders: not checked — {e}"),
                    }
                }
                // A failed machine check (a mismatched binary pair, an unwritable state dir) fails
                // the doctor in either mode; a warning never does.
                if environment
                    .iter()
                    .any(|c| c.level == preflight::Level::Fail)
                {
                    std::process::exit(1);
                }
                // A non-zero exit for a failing `--adapter`, so §8's "Run `--adapter` in CI" is a
                // gate rather than a log. A harness missing under `--capabilities` is not a
                // failure: nothing being installed is an answer, not an error.
                if rows.iter().any(|r| r.report.adapter_check == Some(false)) {
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("marion doctor: {e}");
                eprintln!("{}", doctor::USAGE);
                std::process::exit(2);
            }
        },
        _ => usage(),
    }
}
