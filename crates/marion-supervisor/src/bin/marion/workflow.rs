//! **`marion workflow`**: the workflows a project and its operator define — `list` them, `show` one,
//! `check` a file before trusting it or running it.

use std::path::Path;
use std::process::ExitCode;

use marion_supervisor::workflow_file::{self, LoadError, Source};

use super::cli::{self, Exit, Place, Word, Words};

/// What `marion workflow` was asked.
#[derive(Debug, Default)]
pub struct WorkflowArgs {
    pub verb: String,
    pub rest: Vec<String>,
    pub place: Place,
}

pub fn parse(argv: &[String]) -> Result<WorkflowArgs, Exit> {
    let mut words = Words::after_verb(argv);
    let mut args = WorkflowArgs::default();
    while let Some(word) = words.next()? {
        match word {
            Word::Flag(f, v) if args.place.take(f, v, &mut words)? => {}
            Word::Flag(f, _) => return Err(cli::unknown(f)),
            Word::Plain(w) if args.verb.is_empty() => args.verb = w.to_string(),
            Word::Plain(w) => args.rest.push(w.to_string()),
        }
    }
    let want = |n: usize, usage: &str| {
        if args.rest.len() == n {
            Ok(())
        } else {
            Err(Exit::Usage(usage.to_string()))
        }
    };
    match args.verb.as_str() {
        "list" => want(0, "`marion workflow list` takes no name")?,
        "show" => want(1, "usage: marion workflow show <name>")?,
        "check" => want(1, "usage: marion workflow check <file>")?,
        "" => return Err(Exit::Usage("say list, show <name> or check <file>".into())),
        other => {
            return Err(Exit::Usage(format!(
                "`{other}` is not a workflow verb; try list, show or check"
            )));
        }
    }
    Ok(args)
}

pub fn main(argv: &[String]) -> Result<ExitCode, Exit> {
    let args = parse(argv)?;
    let Some((repo, _state)) = super::resolve_project(&args.place) else {
        return Ok(ExitCode::FAILURE);
    };
    let user = workflow_file::user_dir();
    let mut out = std::io::stdout().lock();
    Ok(run(&args, &repo, user.as_deref(), &mut out))
}

/// The verbs over an explicit tree and user directory, writing to `out`.
pub fn run(
    args: &WorkflowArgs,
    repo: &Path,
    user: Option<&Path>,
    out: &mut dyn std::io::Write,
) -> ExitCode {
    let mut say = |line: String| {
        let _ = writeln!(out, "{line}");
    };
    match args.verb.as_str() {
        "list" => {
            let found = workflow_file::all(repo, user);
            if found.is_empty() {
                say(format!(
                    "no workflows; add one as {}/<name>.toml",
                    workflow_file::repo_dir(repo).display()
                ));
                return ExitCode::SUCCESS;
            }
            for f in found {
                let state = match workflow_file::check(repo, &f.path) {
                    Err(e) => format!("invalid: {}", short(&e)),
                    Ok(_) if f.source == Source::User => "yours".to_string(),
                    Ok((text, _)) => {
                        match marion_supervisor::trust::require_workflow(&f.path, &text) {
                            Ok(()) => "trusted".into(),
                            Err(marion_supervisor::trust::TrustError::UntrustedWorkflow {
                                edited: true,
                                ..
                            }) => "edited since trusted".into(),
                            Err(_) => "not trusted".into(),
                        }
                    }
                };
                say(format!(
                    "{:<16} {:<5} {:<22} {}",
                    f.name,
                    f.source.word(),
                    state,
                    f.path.display()
                ));
            }
            ExitCode::SUCCESS
        }
        "show" => {
            let name = &args.rest[0];
            let Some(found) = workflow_file::find(repo, user, name) else {
                eprintln!(
                    "marion: {}",
                    LoadError::NotFound {
                        name: name.clone(),
                        repo: workflow_file::repo_dir(repo),
                        user: user.map(Path::to_path_buf),
                    }
                );
                return ExitCode::FAILURE;
            };
            describe_file(repo, &found.path, &mut say)
        }
        "check" => describe_file(repo, Path::new(&args.rest[0]), &mut say),
        _ => ExitCode::from(2),
    }
}

/// A file's check, and on success what it runs.
fn describe_file(repo: &Path, path: &Path, say: &mut dyn FnMut(String)) -> ExitCode {
    // A repository's file is checked against its own tree, whatever directory this runs in.
    let tree = if workflow_file::is_repo_workflow(path) {
        path.parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .unwrap_or(repo)
    } else {
        repo
    };
    match workflow_file::check(tree, path) {
        Ok((text, workflow)) => {
            say(format!(
                "{}  {}  sha256 {}",
                workflow.name,
                path.display(),
                marion_supervisor::inbox::sha256_hex(text.as_bytes())
            ));
            for line in workflow_file::describe(&workflow) {
                say(format!("  {line}"));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("marion: {e}");
            ExitCode::FAILURE
        }
    }
}

/// An error's own sentence, without the path a listing already shows.
fn short(e: &LoadError) -> String {
    match e {
        LoadError::Invalid { error, .. } => error.to_string(),
        LoadError::Read { why, .. } => why.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        std::iter::once("workflow")
            .chain(words.iter().copied())
            .map(String::from)
            .collect()
    }

    fn output(args: &WorkflowArgs, repo: &Path, user: &Path) -> (ExitCode, String) {
        let mut out = Vec::new();
        let code = run(args, repo, Some(user), &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    /// **`list` names each workflow once with where it lives and whether it may run; `show` and
    /// `check` say what it runs** — and a file that is not a workflow is refused by key path.
    #[test]
    fn list_show_and_check_say_what_each_workflow_runs() {
        let dir = marion_testsupport::scratch("wf-cli");
        let (repo, user) = (dir.join("repo"), dir.join("user"));
        let wf = "schema = 1\nname = \"plan\"\n[[step]]\nid = \"a\"\nkind = \"agent\"\non = \"claude\"\nprompt = \"p\"\n";
        std::fs::create_dir_all(workflow_file::repo_dir(&repo)).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(workflow_file::repo_dir(&repo).join("plan.toml"), wf).unwrap();
        std::fs::write(user.join("mine.toml"), wf.replace("\"plan\"", "\"mine\"")).unwrap();
        std::fs::write(
            workflow_file::repo_dir(&repo).join("broken.toml"),
            "schema = 7\nname = \"broken\"\n",
        )
        .unwrap();

        let (code, listed) = output(&parse(&argv(&["list"])).unwrap(), &repo, &user);
        assert_eq!(code, ExitCode::SUCCESS);
        let lines: Vec<&str> = listed.lines().collect();
        assert_eq!(lines.len(), 3, "{listed}");
        assert!(
            lines[0].starts_with("broken") && lines[0].contains("invalid: schema"),
            "{listed}"
        );
        assert!(
            lines[1].starts_with("mine") && lines[1].contains("yours"),
            "{listed}"
        );
        assert!(
            lines[2].starts_with("plan") && lines[2].contains("repo"),
            "{listed}"
        );

        let (code, shown) = output(&parse(&argv(&["show", "plan"])).unwrap(), &repo, &user);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(shown.contains("step 1 a: agent on claude"), "{shown}");
        let broken = workflow_file::repo_dir(&repo).join("broken.toml");
        let (code, _) = output(
            &parse(&argv(&["check", &broken.display().to_string()])).unwrap(),
            &repo,
            &user,
        );
        assert_eq!(code, ExitCode::FAILURE);
        for bad in [&["list", "x"][..], &["show"], &["run"], &[]] {
            assert!(parse(&argv(bad)).is_err(), "{bad:?}");
        }
    }
}
