//! **`marion harness`**: the harness rows the operator defines in files — `list` them with whether
//! each loaded, `check` one before relying on it.

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
        let _ = writeln!(
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
        let _ = writeln!(out, "{:<16} loaded   {}", h.as_str(), path.display());
    }
    for e in &installed.refused {
        let name = e.path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        let _ = writeln!(out, "{name:<16} refused  {e}");
    }
    for (h, path) in &repo.loaded {
        let _ = writeln!(
            out,
            "{:<16} loaded   {} (trusted)",
            h.as_str(),
            path.display()
        );
    }
    for e in &repo.refused {
        let _ = writeln!(out, "{:<16} refused  {e}", "(repository)");
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
        eprintln!(
            "marion: {}: `{stem}` is not a name a row file may have: lowercase letters, digits and \
             single dashes, from a letter, and none of a harness marion ships",
            path.display()
        );
        return ExitCode::FAILURE;
    };
    match row_file::build(path, harness) {
        Ok(spec) => {
            let _ = writeln!(
                out,
                "{}  {}  loads: program {}",
                stem,
                path.display(),
                spec.program.unwrap_or("?")
            );
            let env: Vec<&str> = spec.env.iter().map(|e| e.key).collect();
            let _ = writeln!(out, "  env: {}", env.join(" "));
            let _ = writeln!(out, "  approval: {:?}", spec.approval);
            let _ = writeln!(out, "  updates: {:?}", spec.updates);
            ExitCode::SUCCESS
        }
        Err(e) => {
            for f in &e.faults {
                eprintln!("marion: {}: {f}", e.path.display());
            }
            ExitCode::FAILURE
        }
    }
}
