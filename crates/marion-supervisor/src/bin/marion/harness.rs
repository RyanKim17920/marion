//! **`marion harness`**: the harness rows the operator defines in files — `list` them with whether
//! each loaded, `check` one before relying on it.

use marion_supervisor::{err, out_to};
use std::path::Path;
use std::process::ExitCode;

use marion_core::harness::Harness;
use marion_harness::row_file;

use super::cli::Exit;

pub fn main(argv: &[String]) -> Result<ExitCode, Exit> {
    let words: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
    let mut out = std::io::stdout().lock();
    match words.as_slice() {
        ["list"] => Ok(list(&mut out)),
        ["check", file] => Ok(check(Path::new(file), &mut out)),
        ["check"] => Err(Exit::Usage("usage: marion harness check <file>".into())),
        [] => Err(Exit::Usage("say list or check <file>".into())),
        [other, ..] => Err(Exit::Usage(format!(
            "`{other}` is not a harness verb; try list or check <file>"
        ))),
    }
}

/// Every row file in the operator's directory and this repository's: loaded, or refused and why.
fn list(out: &mut dyn std::io::Write) -> ExitCode {
    let installed = row_file::install_user_rows();
    let repo = marion_supervisor::current_repo()
        .map(|r| marion_supervisor::install_repo_rows(&r))
        .unwrap_or_default();
    let dir = row_file::user_dir();
    if installed.loaded.is_empty()
        && installed.refused.is_empty()
        && repo.loaded.is_empty()
        && repo.refused.is_empty()
    {
        let _ = out_to!(
            out,
            "no harness rows; add one as {}/<name>.toml",
            dir.map_or_else(
                || "~/.config/marion/harnesses".into(),
                |d| d.display().to_string()
            )
        );
        return ExitCode::SUCCESS;
    }
    for (h, path) in &installed.loaded {
        let _ = out_to!(out, "{:<16} loaded   {}", h.as_str(), path.display());
    }
    for e in &installed.refused {
        let name = e.path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        let _ = out_to!(out, "{name:<16} refused  {e}");
    }
    for (h, path) in &repo.loaded {
        let _ = out_to!(
            out,
            "{:<16} loaded   {} (trusted)",
            h.as_str(),
            path.display()
        );
    }
    for e in &repo.refused {
        let _ = out_to!(out, "{:<16} refused  {e}", "(repository)");
    }
    ExitCode::SUCCESS
}

/// **Check one file as it would load**, and on success say what it launches.
fn check(path: &Path, out: &mut dyn std::io::Write) -> ExitCode {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let Some(harness) = Harness::named(stem).filter(|h| !h.is_builtin()) else {
        err!(
            "marion: {}: `{stem}` is not a name a row file may have: lowercase letters, digits and \
             single dashes, from a letter, and none of a harness marion ships",
            path.display()
        );
        return ExitCode::FAILURE;
    };
    // The bytes shown are the bytes built: one read, so the review is of what would load.
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            err!("marion: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    match row_file::build_from(path, &text, harness) {
        Ok(spec) => {
            let _ = out_to!(out, "{}  {}  loads:", stem, path.display());
            for line in row_file::describe(spec, &text) {
                let _ = out_to!(out, "  {line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            for f in &e.faults {
                err!("marion: {}: {f}", e.path.display());
            }
            ExitCode::FAILURE
        }
    }
}
