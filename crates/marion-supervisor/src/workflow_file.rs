//! **Finding, reading, checking and trusting a workflow file.**
//!
//! A workflow lives in a repository (`.marion/workflows/<name>.toml`, a tracked file of whatever
//! the operator cloned) or in the operator's own configuration (`$XDG_CONFIG_HOME/marion/
//! workflows/<name>.toml`). The operator's own wins a name both define, since a cloned repository
//! must not be able to shadow a workflow the operator wrote.
//!
//! A repository's file runs only after the operator has read it and said so, exactly as a
//! repository's command-naming agent types do ([`crate::trust`]): `marion trust allow <file>`
//! shows what the workflow runs and records the SHA-256 of its bytes; any edit revokes it. The
//! operator's own files need no trust. What is checked here, before anything is journaled: the file
//! parses as a workflow ([`marion_core::workflow::parse`]), its stem is its name, and every agent
//! type it names exists in the tree's table.

use std::path::{Path, PathBuf};

use marion_core::workflow::{LandMode, StepKind, Workflow};

/// Where a workflow file came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The operator's own configuration: trusted as the operator's words.
    User,
    /// A repository's tracked file: runs once trusted by its content.
    Repo,
}

impl Source {
    pub fn word(self) -> &'static str {
        match self {
            Source::User => "user",
            Source::Repo => "repo",
        }
    }
}

/// A workflow file found by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub path: PathBuf,
    pub source: Source,
}

/// A workflow read and checked, with what it was read from.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub found: Found,
    pub text: String,
    /// SHA-256 of `text`, lowercase hex.
    pub digest: String,
    pub workflow: Workflow,
}

/// The repository's workflow directory under `tree`.
pub fn repo_dir(tree: &Path) -> PathBuf {
    tree.join(".marion").join("workflows")
}

/// The operator's workflow directory, where one can be named.
pub fn user_dir() -> Option<PathBuf> {
    crate::credentials::config_dir()
        .ok()
        .map(|d| d.join("workflows"))
}

/// Where to look, in the order a name is resolved: the operator's own first.
fn dirs(tree: &Path, user: Option<&Path>) -> Vec<(Source, PathBuf)> {
    let mut dirs = Vec::new();
    if let Some(u) = user {
        dirs.push((Source::User, u.to_path_buf()));
    }
    dirs.push((Source::Repo, repo_dir(tree)));
    dirs
}

/// The workflow named `name`, where one is defined.
pub fn find(tree: &Path, user: Option<&Path>, name: &str) -> Option<Found> {
    if !marion_core::agent_type::is_valid_name(name) {
        return None;
    }
    dirs(tree, user).into_iter().find_map(|(source, dir)| {
        let path = dir.join(format!("{name}.toml"));
        path.is_file().then(|| Found {
            name: name.to_string(),
            path,
            source,
        })
    })
}

/// Every workflow file either place holds, one per name (the operator's own shadowing the
/// repository's), sorted by name.
pub fn all(tree: &Path, user: Option<&Path>) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    for (source, dir) in dirs(tree, user) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let Some(name) = path
                .extension()
                .filter(|x| *x == "toml")
                .and_then(|_| path.file_stem())
                .and_then(|s| s.to_str())
            else {
                continue;
            };
            if found.iter().any(|f| f.name == name) {
                continue;
            }
            found.push(Found {
                name: name.to_string(),
                path: path.clone(),
                source,
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// **The names of every workflow that may run as it stands**, sorted: each that [`load`] would
/// load — the operator's own, and the repository's trusted by their current bytes. What the
/// operator's MCP client is offered.
pub fn runnable_names(tree: &Path, user: Option<&Path>) -> Vec<String> {
    all(tree, user)
        .into_iter()
        .filter(|f| load(tree, user, &f.name).is_ok())
        .map(|f| f.name)
        .collect()
}

/// Why a workflow cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    #[error("no workflow named `{name}` in {} or {}", repo.display(), user.as_deref().map_or_else(|| "the user configuration".to_string(), |u| u.display().to_string()))]
    NotFound {
        name: String,
        repo: PathBuf,
        user: Option<PathBuf>,
    },
    #[error("{}: {why}", path.display())]
    Read { path: PathBuf, why: String },
    #[error("{}: {error}", path.display())]
    Invalid {
        path: PathBuf,
        error: marion_core::workflow::WorkflowError,
    },
    #[error(transparent)]
    Untrusted(#[from] crate::trust::TrustError),
}

/// **Read and check `path`** as a workflow of `tree`: the file (never through a link out of its
/// directory, for a repository's), the workflow it parses as, its stem against its name, and every
/// agent type it names against the tree's table. No trust is asked: that is [`load`]'s.
pub fn check(tree: &Path, path: &Path) -> Result<(String, Workflow), LoadError> {
    let text = read(path)?;
    let invalid = |key_path: &str, msg: String| LoadError::Invalid {
        path: path.to_path_buf(),
        error: marion_core::workflow::WorkflowError {
            key_path: key_path.into(),
            msg,
        },
    };
    let workflow = marion_core::workflow::parse(&text).map_err(|error| LoadError::Invalid {
        path: path.to_path_buf(),
        error,
    })?;
    if path.file_stem().and_then(|s| s.to_str()) != Some(workflow.name.as_str()) {
        return Err(invalid(
            "name",
            format!(
                "`{}` is not this file's name; a workflow is named by its file, {}.toml",
                workflow.name,
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
            ),
        ));
    }
    let types = crate::run::agent_types(tree).map_err(|e| LoadError::Read {
        path: path.to_path_buf(),
        why: e.to_string(),
    })?;
    for (i, step) in workflow.steps.iter().enumerate() {
        for c in step.kind.agents() {
            if types.resolve(&c.agent_type).is_none() {
                return Err(invalid(
                    &format!("step[{i}].on"),
                    format!(
                        "no agent type is named `{}`; this tree's are {}",
                        c.agent_type,
                        types.names().join(", ")
                    ),
                ));
            }
        }
    }
    Ok((text, workflow))
}

/// A file's text, refusing a link that leaves its directory (a repository's file must be the
/// repository's own) and anything that is not UTF-8.
fn read(path: &Path) -> Result<String, LoadError> {
    let refuse = |why: String| LoadError::Read {
        path: path.to_path_buf(),
        why,
    };
    if let (Ok(real), Some(dir)) = (path.canonicalize(), path.parent())
        && let Ok(dir) = dir.canonicalize()
        && !real.starts_with(&dir)
    {
        return Err(refuse(format!(
            "it is a link to {}, outside the directory it is listed in",
            real.display()
        )));
    }
    std::fs::read_to_string(path).map_err(|e| refuse(e.to_string()))
}

/// **The workflow named `name`, ready to run**: found, checked, and — a repository's — trusted by
/// the digest of the very text that was checked.
pub fn load(tree: &Path, user: Option<&Path>, name: &str) -> Result<Loaded, LoadError> {
    let found = find(tree, user, name).ok_or_else(|| LoadError::NotFound {
        name: name.to_string(),
        repo: repo_dir(tree),
        user: user.map(Path::to_path_buf),
    })?;
    let (text, workflow) = check(tree, &found.path)?;
    if found.source == Source::Repo {
        crate::trust::require_workflow(&found.path, &text)?;
    }
    Ok(Loaded {
        digest: crate::inbox::sha256_hex(text.as_bytes()),
        found,
        text,
        workflow,
    })
}

/// **What a workflow runs, for the operator to read** before trusting it or running it: each step
/// and every agent type it starts, and every command it runs as verification.
pub fn describe(workflow: &Workflow) -> Vec<String> {
    let mut lines = Vec::new();
    if !workflow.inputs.is_empty() {
        lines.push(format!("inputs: {}", workflow.inputs.join(", ")));
    }
    let mut budget = Vec::new();
    if let Some(t) = workflow.budget.tokens {
        budget.push(format!("{t} tokens"));
    }
    if let Some(w) = workflow.budget.wall_secs {
        budget.push(format!("{w} s"));
    }
    if !budget.is_empty() {
        lines.push(format!("budget: {}", budget.join(" · ")));
    }
    let id = |i: usize| workflow.steps[i].id.clone();
    let labels = |on: &[&marion_core::race::Candidate]| {
        on.iter().map(|c| c.label()).collect::<Vec<_>>().join(", ")
    };
    let mut commands: Vec<&str> = Vec::new();
    for (i, step) in workflow.steps.iter().enumerate() {
        let what = match &step.kind {
            StepKind::Agent {
                on,
                read_only,
                verify,
                ..
            } => {
                commands.extend(verify.iter().map(String::as_str));
                format!(
                    "agent on {}{}",
                    on.label(),
                    if *read_only { " (read-only)" } else { "" }
                )
            }
            StepKind::Parallel { on, .. } => {
                format!(
                    "parallel on {} (read-only)",
                    labels(&on.iter().collect::<Vec<_>>())
                )
            }
            StepKind::Race { on, verify, .. } => {
                commands.extend(verify.iter().map(String::as_str));
                format!("race on {}", labels(&on.iter().collect::<Vec<_>>()))
            }
            StepKind::Review { of, max_rounds, on } => format!(
                "review of {} by {}, up to {max_rounds} round(s)",
                id(*of),
                on.as_ref().map_or_else(
                    || "a reviewer from another model family".into(),
                    |c| c.label()
                )
            ),
            StepKind::Land { of, mode } => format!(
                "land {} {}",
                id(*of),
                match mode {
                    LandMode::Branch => "on its branch",
                    LandMode::Ff => "by fast-forward",
                }
            ),
        };
        let when = step
            .when
            .map(|w| format!(", when {}:{}", id(w.step), w.verdict.word()))
            .unwrap_or_default();
        lines.push(format!("step {} {}: {what}{when}", i + 1, step.id));
    }
    commands.sort_unstable();
    commands.dedup();
    if !commands.is_empty() {
        lines.push(format!("runs as verification: {}", commands.join(" · ")));
    }
    lines
}

/// Whether `path` is a workflow file of a repository (`…/.marion/workflows/<name>.toml`) — what
/// `marion trust allow` describes as a workflow rather than as an agents file.
pub fn is_repo_workflow(path: &Path) -> bool {
    path.extension().is_some_and(|x| x == "toml")
        && path
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n == "workflows"))
            .and_then(Path::parent)
            .is_some_and(|p| p.file_name().is_some_and(|n| n == ".marion"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "schema = 1\nname = \"plan\"\ninputs = [\"task\"]\n\n[[step]]\nid = \"think\"\nkind = \"agent\"\non = \"claude\"\nread_only = true\nprompt = \"Plan {input.task}\"\n\n[[step]]\nid = \"do\"\nkind = \"agent\"\non = \"codex\"\nprompt = \"{think.report}\"\nverify = [\"cargo test\"]\n";

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// **A name resolves to the operator's own file before a repository's**, and a listing shows
    /// each name once, the operator's where both define it.
    #[test]
    fn the_operators_own_file_shadows_a_repositorys_of_the_same_name() {
        let dir = marion_testsupport::scratch("wf-find");
        let (tree, user) = (dir.join("tree"), dir.join("user"));
        write(&repo_dir(&tree).join("plan.toml"), PLAN);
        write(&repo_dir(&tree).join("other.toml"), PLAN);
        write(&user.join("plan.toml"), PLAN);
        let f = find(&tree, Some(&user), "plan").unwrap();
        assert_eq!(f.source, Source::User);
        assert_eq!(find(&tree, None, "plan").unwrap().source, Source::Repo);
        assert_eq!(find(&tree, Some(&user), "../plan"), None, "never a path");
        let names: Vec<_> = all(&tree, Some(&user))
            .into_iter()
            .map(|f| (f.name, f.source))
            .collect();
        assert_eq!(
            names,
            [
                ("other".to_string(), Source::Repo),
                ("plan".to_string(), Source::User)
            ]
        );
    }

    /// **A file is checked whole before anything runs**: its stem is its name, and every agent
    /// type it starts exists in the tree.
    #[test]
    fn a_file_is_refused_for_a_wrong_name_or_an_unknown_agent_type() {
        let dir = marion_testsupport::scratch("wf-check");
        let tree = dir.join("tree");
        let good = repo_dir(&tree).join("plan.toml");
        write(&good, PLAN);
        let (_, wf) = check(&tree, &good).unwrap();
        assert_eq!(wf.steps.len(), 2);
        let misnamed = repo_dir(&tree).join("other.toml");
        write(&misnamed, PLAN);
        let e = check(&tree, &misnamed).unwrap_err();
        assert!(
            e.to_string()
                .contains("name: `plan` is not this file's name"),
            "{e}"
        );
        let unknown = repo_dir(&tree).join("plan.toml");
        write(&unknown, &PLAN.replace("\"codex\"", "\"nosuch\""));
        let e = check(&tree, &unknown).unwrap_err();
        assert!(
            e.to_string()
                .contains("step[1].on: no agent type is named `nosuch`"),
            "{e}"
        );
    }

    /// **What a workflow runs, in words**: each step, each agent type, every verification command.
    #[test]
    fn describe_names_every_step_agent_and_command() {
        let wf = marion_core::workflow::parse(PLAN).unwrap();
        let lines = describe(&wf);
        assert_eq!(
            lines,
            [
                "inputs: task",
                "step 1 think: agent on claude (read-only)",
                "step 2 do: agent on codex",
                "runs as verification: cargo test",
            ]
        );
        assert!(is_repo_workflow(Path::new(
            "/r/.marion/workflows/plan.toml"
        )));
        assert!(!is_repo_workflow(Path::new("/r/.marion/agents.toml")));
    }
}
