//! **`marion harness`: harness rows from the operator's own files**, through the real binary: a
//! row file in `$XDG_CONFIG_HOME/marion/harnesses/` is loaded by every command, `list` says
//! whether each loaded, and `check` refuses a file by the key it breaks.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use marion_testsupport::scratch;

/// The goose twin the harness crate holds byte for byte to the built-in goose row.
fn goose_twin() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../marion-harness/tests/fixtures/rows/goose-twin.toml"
    ))
    .unwrap()
}

/// `text` as the row file `<name>.toml` in `dir`, with `mode`.
fn row(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{name}.toml"));
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn marion(config: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args(args)
        .env("XDG_CONFIG_HOME", config)
        .output()
        .expect("marion runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// **A row file in the operator's directory loads in every command, and one that is not theirs
/// alone is refused by name** — `list` shows both, and the refusal reaches stderr of any command.
#[test]
fn the_operators_row_files_load_and_a_writable_one_is_refused() {
    let dir = scratch("harness-rows-list");
    let config = dir.join("config");
    let rows = config.join("marion").join("harnesses");
    let twin = goose_twin().replace("name = \"goose-twin\"", "name = \"goose-mine\"");
    row(&rows, "goose-mine", &twin, 0o600);
    let shared = twin.replace("name = \"goose-mine\"", "name = \"goose-shared\"");
    row(&rows, "goose-shared", &shared, 0o666);

    let (code, stdout, stderr) = marion(&config, &["harness", "list"]);
    assert_eq!(code, Some(0), "{stdout}\n{stderr}");
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("goose-mine") && l.contains("loaded")),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("goose-shared")
                && l.contains("writable by its group or by others")),
        "{stdout}"
    );
    assert!(
        stderr.contains("harness row not loaded") && stderr.contains("goose-shared"),
        "every command says which row it refused: {stderr}"
    );
}

/// **`check` says what a good file runs and refuses a broken one by key, exiting 1.**
#[test]
fn check_names_what_a_row_runs_or_the_key_it_breaks() {
    let dir = scratch("harness-rows-check");
    let config = dir.join("config");
    let good = row(
        &dir.join("files"),
        "goose-check",
        &goose_twin().replace("name = \"goose-twin\"", "name = \"goose-check\""),
        0o600,
    );
    let (code, stdout, stderr) = marion(&config, &["harness", "check", good.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{stdout}\n{stderr}");
    assert!(stdout.contains("loads: program goose"), "{stdout}");
    assert!(stdout.contains("GOOSE_MODE"), "{stdout}");

    let leaky = row(
        &dir.join("files"),
        "goose-leaky",
        &goose_twin()
            .replace("name = \"goose-twin\"", "name = \"goose-leaky\"")
            .replace(r#"{ lit = "-q" },"#, r#"{ flag = ["--key", "api-key"] },"#),
        0o600,
    );
    let (code, _, stderr) = marion(&config, &["harness", "check", leaky.to_str().unwrap()]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("argv: ApiKey may not ride argv"),
        "{stderr}"
    );

    let shadow = row(&dir.join("files"), "codex", &goose_twin(), 0o600);
    let (code, _, stderr) = marion(&config, &["harness", "check", shadow.to_str().unwrap()]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("none of a harness marion ships"),
        "{stderr}"
    );
}
