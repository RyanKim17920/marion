//! **`marion notify`** through the real binary: off by default, `on` and `off` written to a private
//! `notify.toml`, and `test` shown through the resolved backend — here the `record:` fake.
//!
//! ```sh
//! cargo test -p marion-supervisor --test notify_cli
//! ```

use std::path::Path;
use std::process::{Command, Output};

mod common;

fn marion(config: &Path, args: &[&str], backend: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    cmd.arg("notify").args(args);
    cmd.env("XDG_CONFIG_HOME", config);
    cmd.env_remove("MARION_NOTIFY");
    cmd.env_remove("MARION_NOTIFY_BACKEND");
    if let Some(b) = backend {
        cmd.env("MARION_NOTIFY_BACKEND", b);
    }
    cmd.output().expect("the marion binary runs")
}

fn stdout(o: &Output) -> String {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn notifications_start_off_turn_on_privately_and_test_through_the_backend() {
    let dir = marion_testsupport::scratch("notify-cli");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();

    let status = stdout(&marion(&config, &["status"], None));
    assert!(status.contains("notifications: off"), "{status}");

    stdout(&marion(&config, &["on"], None));
    let file = config.join("marion").join("notify.toml");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o077,
        0,
        "private to the operator"
    );
    assert!(stdout(&marion(&config, &["status"], None)).contains("notifications: on"));

    let record = dir.join("notices.jsonl");
    let tested = stdout(&marion(
        &config,
        &["test"],
        Some(&format!("record:{}", record.display())),
    ));
    assert!(tested.contains("record:"), "{tested}");
    assert_eq!(
        std::fs::read_to_string(&record).unwrap().lines().count(),
        1,
        "one notice recorded"
    );

    stdout(&marion(&config, &["off"], None));
    assert!(stdout(&marion(&config, &["status"], None)).contains("notifications: off"));
    assert_eq!(marion(&config, &["loud"], None).status.code(), Some(2));
}

/// **`marion notify on|off` reaches this project's running supervisor**, which turns its notices
/// on or off at once (`notify/configure`) rather than when it next starts; with none running it
/// only writes the file, and starts nothing.
#[test]
fn notify_on_and_off_reach_the_projects_running_supervisor() {
    let dir = marion_testsupport::scratch("notify-running");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let repo = marion_testsupport::fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let place = |args: &[&str]| -> Vec<String> {
        args.iter()
            .map(|a| a.to_string())
            .chain(["--repo".into(), repo.display().to_string()])
            .chain(["--state-dir".into(), state.display().to_string()])
            .collect()
    };
    let run = |args: Vec<String>| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        stdout(&marion(&config, &refs, None))
    };
    let alone = run(place(&["on"]));
    assert!(
        !alone.contains("running supervisor"),
        "none is running: {alone}"
    );

    let mut sup = common::Supervisor::start(
        &state,
        &marion_supervisor::socket::project_root(&repo),
        &std::env::var("PATH").unwrap_or_default(),
        "http://127.0.0.1:9/v1",
        std::time::Duration::from_secs(30),
    );
    let off = run(place(&["off"]));
    assert!(
        off.contains("this project's running supervisor has them off now"),
        "{off}"
    );
    let on = run(place(&["on"]));
    assert!(
        on.contains("this project's running supervisor has them on now"),
        "{on}"
    );
    sup.stop();
}
