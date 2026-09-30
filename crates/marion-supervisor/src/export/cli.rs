//! `marion export <agent-id|short-id> [--md|--html] [-o <file>] [--include-prompt] [--full-diff]
//! [--timeline all|<n>] [--repo <path>] [--state-dir <path>]`.
//!
//! Reads the project's files and writes the report; it starts no supervisor, dials none, and
//! works the same with one serving or with none.

use std::path::PathBuf;
use std::process::ExitCode;

use super::model::{ExportOpts, Format, TimelineMode};

/// The verb's usage line, for the top-level usage text and for a refusal here.
pub const USAGE: &str = "marion export <agent-id|short-id> [--md|--html] [-o <file>] [--include-prompt] \
                         [--full-diff] [--timeline all|<n>] [--repo <path>] [--state-dir <path>]";

/// `marion export`'s arguments, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub target: String,
    /// Asked for by flag; else [`Format::of_path`] of `out`; else Markdown.
    pub format: Option<Format>,
    /// `None` (or `-`) is stdout.
    pub out: Option<PathBuf>,
    pub opts: ExportOpts,
    pub repo: Option<PathBuf>,
    pub state_dir: Option<String>,
}

impl Args {
    /// The format the report is written in.
    pub fn format(&self) -> Format {
        self.format
            .or_else(|| self.out.as_deref().map(Format::of_path))
            .unwrap_or(Format::Markdown)
    }
}

/// `argv` after the verb. Every refusal names what was wrong.
pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut target = None;
    let mut format = None;
    let mut out = None;
    let mut opts = ExportOpts::default();
    let (mut repo, mut state_dir) = (None, None);
    let mut rest = argv.iter();
    let set_format = |f: Format, format: &mut Option<Format>| match format {
        Some(other) if *other != f => Err("--md and --html cannot both be given".to_string()),
        _ => {
            *format = Some(f);
            Ok(())
        }
    };
    while let Some(word) = rest.next() {
        let mut value = |flag: &str| {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match word.as_str() {
            "--md" | "--markdown" => set_format(Format::Markdown, &mut format)?,
            "--html" => set_format(Format::Html, &mut format)?,
            "-o" | "--output" => out = Some(PathBuf::from(value(word)?)).filter(|p| p != "-"),
            "--include-prompt" => opts.include_prompt = true,
            "--full-diff" => opts.full_diff = true,
            "--timeline" => {
                opts.timeline = match value(word)?.as_str() {
                    "all" => TimelineMode::All,
                    n => TimelineMode::Condensed(n.parse().map_err(|_| {
                        format!("--timeline takes `all` or a number of actions, not `{n}`")
                    })?),
                }
            }
            "--repo" => repo = Some(PathBuf::from(value(word)?)),
            "--state-dir" => state_dir = Some(value(word)?),
            f if f.starts_with('-') => return Err(format!("unknown flag `{f}`")),
            id if target.is_none() => target = Some(id.to_string()),
            extra => return Err(format!("one node at a time; `{extra}` is a second")),
        }
    }
    Ok(Args {
        target: target.ok_or("name the node to report on: its id or the short id its row shows")?,
        format,
        out,
        opts,
        repo,
        state_dir,
    })
}

/// The verb. `resolve` is the CLI's own repository and state-dir resolution, the one every verb
/// uses; it prints its own refusal.
pub fn main(
    argv: &[String],
    resolve: impl FnOnce(Option<PathBuf>, Option<&str>) -> Option<(PathBuf, PathBuf)>,
) -> ExitCode {
    let args = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("marion export: {e}\nusage: {USAGE}");
            return ExitCode::from(2);
        }
    };
    let Some((repo, state)) = resolve(args.repo.clone(), args.state_dir.as_deref()) else {
        return ExitCode::FAILURE;
    };
    match export(&args, &repo, &state) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("marion export: {e}");
            ExitCode::FAILURE
        }
    }
}

fn export(args: &Args, repo: &std::path::Path, state: &std::path::Path) -> Result<(), String> {
    let project = marion_core::paths::ProjectDir::new(state, &crate::socket::project_root(repo));
    let now = marion_core::encoding::SystemTime(std::time::SystemTime::now());
    let collected = super::collect::collect(&project, repo, &args.target, &args.opts, now)?;
    // The operator's credential store is opened only when an endpoint node needs a key looked up,
    // so exporting a tree of subscription-login nodes never touches the Keychain.
    let keys = if collected.credentials.is_empty() {
        Vec::new()
    } else {
        let store = crate::credentials::default_store().map_err(|e| e.to_string())?;
        super::keys_behind(&collected.credentials, store.as_ref())?
    };
    let scrubber = super::scrub::Scrubber::from_process(keys);
    let nodes = collected.report.nodes.len();
    let text = super::render(collected.report, &scrubber, args.format())?;
    match &args.out {
        Some(path) => {
            super::write_private(path, &text)
                .map_err(|e| format!("writing {}: {e}", path.display()))?;
            eprintln!("marion: wrote {} ({nodes} nodes)", path.display());
        }
        None => {
            use std::io::Write;
            // To a terminal as `marion ls` prints: node-authored text carries no escape sequence.
            std::io::stdout()
                .write_all(crate::printable::printable(&text).as_bytes())
                .map_err(|e| format!("writing the report: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_format_is_the_flag_else_the_file_extension_else_markdown() {
        let a = parse(&argv(&["8ea3"])).unwrap();
        assert_eq!((a.format(), a.out.clone()), (Format::Markdown, None));
        assert_eq!(
            parse(&argv(&["8ea3", "-o", "r.HTML"])).unwrap().format(),
            Format::Html
        );
        assert_eq!(
            parse(&argv(&["8ea3", "--md", "-o", "r.html"]))
                .unwrap()
                .format(),
            Format::Markdown
        );
        assert_eq!(parse(&argv(&["8ea3", "-o", "-"])).unwrap().out, None);
        assert!(parse(&argv(&["8ea3", "--md", "--html"])).is_err());
    }

    #[test]
    fn every_option_is_read_and_every_mistake_is_named() {
        let a = parse(&argv(&[
            "--include-prompt",
            "8ea3",
            "--full-diff",
            "--timeline",
            "all",
            "--repo",
            "/r",
            "--state-dir",
            "/s",
        ]))
        .unwrap();
        assert_eq!(a.target, "8ea3");
        assert!(a.opts.include_prompt && a.opts.full_diff);
        assert_eq!(a.opts.timeline, TimelineMode::All);
        assert_eq!(
            (a.repo, a.state_dir),
            (Some(PathBuf::from("/r")), Some("/s".into()))
        );
        let default = parse(&argv(&["8ea3"])).unwrap().opts;
        assert!(
            !default.include_prompt,
            "the prompt is withheld unless asked for"
        );
        assert_eq!(
            parse(&argv(&["8ea3", "--timeline", "12"]))
                .unwrap()
                .opts
                .timeline,
            TimelineMode::Condensed(12)
        );
        for (bad, says) in [
            (vec![], "name the node"),
            (vec!["a", "b"], "one node at a time"),
            (vec!["a", "--nope"], "unknown flag `--nope`"),
            (vec!["a", "--timeline", "lots"], "not `lots`"),
            (vec!["a", "-o"], "-o needs a value"),
        ] {
            let e = parse(&argv(&bad)).unwrap_err();
            assert!(e.contains(says), "{bad:?}: {e}");
        }
    }
}
