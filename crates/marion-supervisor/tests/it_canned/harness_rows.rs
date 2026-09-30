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

/// `marion <args>` in `cwd`, with its own config and data homes (the trust store is under data).
fn marion_in(
    cwd: &Path,
    config: &Path,
    data: &Path,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args(args)
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_DATA_HOME", data)
        .output()
        .expect("marion runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// **A repository's row loads only once its bytes are trusted**, through the one trust store:
/// untrusted it is refused with the allow command, `marion trust allow` shows what it runs and
/// trusts it, an edit revokes it, and a row of the operator's own by the same name wins with the
/// refusal naming both files.
#[test]
fn a_repository_row_loads_only_once_its_bytes_are_trusted() {
    let dir = scratch("harness-rows-repo");
    let repo = marion_testsupport::fixture_repo(&dir);
    let (config, data) = (dir.join("config"), dir.join("data"));
    std::fs::create_dir_all(&data).unwrap();
    let text = goose_twin().replace("name = \"goose-twin\"", "name = \"goose-repo\"");
    let file = row(
        &repo.join(".marion").join("harnesses"),
        "goose-repo",
        &text,
        0o600,
    );
    let list = || marion_in(&repo, &config, &data, &["harness", "list"]);

    let (_, stdout, stderr) = list();
    assert!(stdout.contains("is not one you have allowed"), "{stdout}");
    assert!(
        stderr.contains("marion trust allow"),
        "every command says how: {stderr}"
    );

    let (code, shown, err) = marion_in(
        &repo,
        &config,
        &data,
        &["trust", "allow", file.to_str().unwrap()],
    );
    assert_eq!(code, Some(0), "{shown}\n{err}");
    for want in [
        "program: goose",
        "env: HOME",
        "GOOSE_MODE",
        "updates: Never",
        "allowed.",
    ] {
        assert!(
            shown.contains(want),
            "`trust allow` shows {want:?}: {shown}"
        );
    }
    let (_, stdout, _) = list();
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("goose-repo") && l.contains("(trusted)")),
        "{stdout}"
    );

    std::fs::write(&file, format!("{text}\n# edited\n")).unwrap();
    let (_, stdout, _) = list();
    assert!(
        stdout.contains("has changed since you allowed it"),
        "{stdout}"
    );

    // The operator's own row by that name wins over the repository's, trusted or not.
    std::fs::write(&file, &text).unwrap();
    marion_in(
        &repo,
        &config,
        &data,
        &["trust", "allow", file.to_str().unwrap()],
    );
    let mine = row(
        &config.join("marion").join("harnesses"),
        "goose-repo",
        &text,
        0o600,
    );
    let (_, stdout, _) = list();
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("goose-repo") && l.contains("loaded") && !l.contains("trusted")),
        "{stdout}"
    );
    assert!(
        stdout.contains("already defined by") && stdout.contains(&mine.display().to_string()),
        "{stdout}"
    );
}
