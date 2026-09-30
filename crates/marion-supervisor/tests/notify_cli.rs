//! **`marion notify`** through the real binary: off by default, `on` and `off` written to a private
//! `notify.toml`, and `test` shown through the resolved backend — here the `record:` fake.
//!
//! ```sh
//! cargo test -p marion-supervisor --test notify_cli
//! ```

use std::path::Path;
use std::process::{Command, Output};

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
