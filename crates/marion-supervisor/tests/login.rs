//! `marion login` / `marion logout`, driven through the real `marion` binary against a throwaway
//! `XDG_CONFIG_HOME` and the file credential store.
//!
//! Every key here is a fixture string. No test reads a real credential, touches the Keychain or
//! starts any vendor's login: the child runs with `MARION_CREDENTIAL_STORE=file`, a scratch `HOME`
//! and every provider's import variable removed from its environment.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use marion_core::provider::PROVIDERS;

const FAKE_KEY: &str = "sk-login-test-DO-NOT-PRINT-0123456789";

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    /// Both streams, for a "never printed" assertion.
    fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marion-login-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn marion(home: &Path, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Run {
    marion_in(home, home, args, stdin, env)
}

fn marion_in(
    home: &Path,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    env: &[(&str, &str)],
) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("MARION_CREDENTIAL_STORE", "file")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for p in PROVIDERS {
        if let Some(var) = p.import_env {
            cmd.env_remove(var);
        }
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("marion starts");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn stored_file(home: &Path) -> PathBuf {
    home.join("xdg").join("marion").join("credentials.json")
}

fn list_row<'a>(run: &'a Run, id: &str) -> &'a str {
    run.stdout
        .lines()
        .find(|l| l.split_whitespace().next() == Some(id))
        .unwrap_or_else(|| panic!("no row for {id} in:\n{}", run.stdout))
}

#[test]
fn a_key_piped_with_stdin_is_stored_in_the_file_backend_and_never_printed() {
    let home = scratch("stdin");
    let run = marion(&home, &["login", "openai", "--stdin"], Some(FAKE_KEY), &[]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(!run.all().contains(FAKE_KEY), "{}", run.all());
    assert!(run.stdout.contains("openai"), "{}", run.stdout);
    let text = std::fs::read_to_string(stored_file(&home)).unwrap();
    assert!(text.contains(FAKE_KEY), "the file backend holds the key");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(stored_file(&home))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn from_env_imports_the_providers_own_variable_with_explicit_consent() {
    let home = scratch("fromenv");
    let run = marion(
        &home,
        &["login", "groq", "--from-env"],
        None,
        &[("GROQ_API_KEY", FAKE_KEY)],
    );
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(!run.all().contains(FAKE_KEY), "{}", run.all());
    assert!(
        std::fs::read_to_string(stored_file(&home))
            .unwrap()
            .contains(FAKE_KEY)
    );
    // Unset is a refusal naming the variable, and stores nothing.
    let home = scratch("fromenv-unset");
    let run = marion(&home, &["login", "groq", "--from-env"], None, &[]);
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("GROQ_API_KEY"), "{}", run.stderr);
    assert!(!stored_file(&home).exists());
}

#[test]
fn without_a_terminal_and_without_a_flag_login_refuses_and_waits_for_nothing() {
    let home = scratch("notty");
    let run = marion(&home, &["login", "openai"], None, &[]);
    assert_ne!(run.code, 0, "{}", run.all());
    assert!(run.stderr.contains("--stdin"), "{}", run.stderr);
    assert!(run.stderr.contains("--from-env"), "{}", run.stderr);
    assert!(!stored_file(&home).exists());
}

#[test]
fn logout_removes_the_key_and_says_so() {
    let home = scratch("logout");
    assert_eq!(
        marion(&home, &["login", "mistral", "--stdin"], Some(FAKE_KEY), &[]).code,
        0
    );
    let run = marion(&home, &["logout", "mistral"], None, &[]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("Removed"), "{}", run.stdout);
    assert!(
        !std::fs::read_to_string(stored_file(&home))
            .unwrap()
            .contains(FAKE_KEY)
    );
    let again = marion(&home, &["logout", "mistral"], None, &[]);
    assert_eq!(again.code, 0);
    assert!(again.stdout.contains("No key"), "{}", again.stdout);
}

#[test]
fn list_shows_every_provider_and_which_are_stored_but_never_the_key() {
    let home = scratch("list");
    assert_eq!(
        marion(
            &home,
            &["login", "openrouter", "--stdin"],
            Some(FAKE_KEY),
            &[]
        )
        .code,
        0
    );
    let run = marion(&home, &["login", "--list"], None, &[]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(!run.all().contains(FAKE_KEY), "{}", run.all());
    for p in PROVIDERS {
        list_row(&run, p.id);
    }
    let or = list_row(&run, "openrouter");
    assert!(or.contains("openai-chat,anthropic"), "{or}");
    assert!(or.trim_end().ends_with("openrouter"), "{or}");
    assert!(list_row(&run, "openai").trim_end().ends_with('-'));
    assert!(list_row(&run, "ollama").contains("not needed"));
}

#[test]
fn an_unknown_provider_is_refused_by_name_with_the_command_to_define_it() {
    let home = scratch("unknown");
    let run = marion(&home, &["login", "nope", "--stdin"], Some(FAKE_KEY), &[]);
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("`nope`"), "{}", run.stderr);
    assert!(run.stderr.contains("marion login custom"), "{}", run.stderr);
    assert!(!run.all().contains(FAKE_KEY));
}

#[test]
fn a_custom_provider_is_added_listed_and_can_hold_a_key() {
    let home = scratch("custom");
    let run = marion(
        &home,
        &[
            "login",
            "custom",
            "my-gw",
            "--base-url",
            "https://gw.example/v1",
            "--wire",
            "openai-chat,anthropic",
        ],
        None,
        &[],
    );
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("marion login my-gw"), "{}", run.stdout);
    let toml = std::fs::read_to_string(home.join("xdg/marion/providers.toml")).unwrap();
    assert!(toml.contains("[providers.my-gw]"), "{toml}");
    assert_eq!(
        marion(&home, &["login", "my-gw", "--stdin"], Some(FAKE_KEY), &[]).code,
        0
    );
    let list = marion(&home, &["login", "--list"], None, &[]);
    let row = list_row(&list, "my-gw");
    assert!(
        row.contains("(custom)") && row.trim_end().ends_with("my-gw"),
        "{row}"
    );
    // A custom id may not shadow a built-in.
    let shadow = marion(
        &home,
        &[
            "login",
            "custom",
            "openai",
            "--base-url",
            "https://evil.example/v1",
            "--wire",
            "openai-chat",
        ],
        None,
        &[],
    );
    assert_ne!(shadow.code, 0);
    assert!(shadow.stderr.contains("built in"), "{}", shadow.stderr);
}

#[test]
fn a_repositorys_own_providers_toml_is_never_read() {
    let home = scratch("repo");
    let repo = home.join("repo");
    std::fs::create_dir_all(repo.join(".marion")).unwrap();
    std::fs::write(
        repo.join(".marion/providers.toml"),
        "[providers.evil]\nbase_url = \"https://evil.example/v1\"\nwires = [\"openai-chat\"]\n",
    )
    .unwrap();
    let list = marion_in(&home, &repo, &["login", "--list"], None, &[]);
    assert_eq!(list.code, 0, "{}", list.all());
    assert!(!list.stdout.contains("evil"), "{}", list.stdout);
    let run = marion_in(
        &home,
        &repo,
        &["login", "evil", "--stdin"],
        Some(FAKE_KEY),
        &[],
    );
    assert_ne!(run.code, 0, "{}", run.all());
}

#[test]
fn several_labelled_keys_for_one_provider_are_stored_listed_and_removed_one_by_one() {
    let home = scratch("labels");
    const WORK: &str = "sk-work-key-DO-NOT-PRINT-1";
    const PERSONAL: &str = "sk-personal-key-DO-NOT-PRINT-2";
    let work = marion(
        &home,
        &["login", "openrouter", "--label", "work", "--stdin"],
        Some(WORK),
        &[],
    );
    assert_eq!(work.code, 0, "{}", work.all());
    assert!(work.stdout.contains("openrouter:work"), "{}", work.stdout);
    let personal = marion(
        &home,
        &["login", "openrouter:personal", "--stdin"],
        Some(PERSONAL),
        &[],
    );
    assert_eq!(personal.code, 0, "{}", personal.all());
    let list = marion(&home, &["login", "--list"], None, &[]);
    assert_eq!(list.code, 0, "{}", list.all());
    let row = list_row(&list, "openrouter");
    assert!(
        row.trim_end()
            .ends_with("openrouter:work, openrouter:personal"),
        "login order, ids only: {row}"
    );
    assert!(!list.all().contains(WORK) && !list.all().contains(PERSONAL));
    let out = marion(
        &home,
        &["logout", "openrouter", "--label", "work"],
        None,
        &[],
    );
    assert_eq!(out.code, 0, "{}", out.all());
    let text = std::fs::read_to_string(stored_file(&home)).unwrap();
    assert!(
        !text.contains(WORK) && text.contains(PERSONAL),
        "only the work key went"
    );
    let list = marion(&home, &["login", "--list"], None, &[]);
    assert!(
        list_row(&list, "openrouter")
            .trim_end()
            .ends_with("openrouter:personal")
    );
    // A label outside the id grammar is refused before anything is read.
    let bad = marion(
        &home,
        &["login", "openrouter", "--label", "Work Laptop", "--stdin"],
        Some(WORK),
        &[],
    );
    assert_ne!(bad.code, 0);
}
