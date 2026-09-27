//! What `doctor` checks about this machine before any harness: the two marion binaries, the OS,
//! the state directory and its socket path, and git.
//!
//! Split in two so the verdicts are testable without the machine: [`gather`] reads the facts
//! (it runs `--version` on marion and git, and creates the state directory as any marion verb
//! would), and [`checks`] turns them into lines, as a pure function.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::socket;

/// Set by `marion doctor` to its own executable before it execs the supervisor, so the doctor
/// reports the marion that was actually typed rather than guessing one.
pub const CLIENT_EXE_ENV: &str = "MARION_CLIENT_EXE";

/// How bad one finding is. Only [`Level::Fail`] makes `doctor` exit non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        }
    }
}

/// One finding: its level and the sentence a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    pub text: String,
}

impl Check {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Check {
            level,
            text: text.into(),
        }
    }

    /// The line as `doctor` prints it under `summary`.
    pub fn line(&self) -> String {
        format!("  {}: {}\n", self.level.word(), self.text)
    }
}

/// Where the supervisor marion starts was found: marion's own rule is beside itself, else `$PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Beside(PathBuf),
    OnPath(PathBuf),
    Missing,
}

/// Everything [`checks`] needs, read by [`gather`].
#[derive(Debug, Clone)]
pub struct Facts {
    /// `marion`'s path and what its `--version` printed (`None` when it could not be run).
    pub marion: Option<(PathBuf, Option<String>)>,
    /// The supervisor marion would start, and that binary's `--version` output.
    pub supervisor: Resolved,
    pub supervisor_version: Option<String>,
    /// `std::env::consts::OS`.
    pub os: String,
    /// The state directory by the one resolver every verb uses, or why there is none.
    pub state_dir: Result<PathBuf, String>,
    /// `Err(why)` when the state directory could not be created or written.
    pub state_writable: Result<(), String>,
    /// This project's socket paths under that state directory.
    pub socket: Option<socket::SocketPaths>,
    /// `git --version`, when git runs.
    pub git: Option<String>,
}

/// Read the facts from this process and machine.
pub fn gather() -> Facts {
    let marion = find_marion().map(|p| {
        let v = version_of(&p);
        (p, v)
    });
    let supervisor = match &marion {
        Some((m, _)) => resolve_supervisor(m),
        None => on_path("marion-supervisor").map_or(Resolved::Missing, Resolved::OnPath),
    };
    let own = std::env::current_exe().ok();
    let supervisor_version = match &supervisor {
        Resolved::Beside(p) | Resolved::OnPath(p) if same_file(Some(p), own.as_deref()) => {
            Some(format!("marion-supervisor {}", env!("CARGO_PKG_VERSION")))
        }
        Resolved::Beside(p) | Resolved::OnPath(p) => version_of(p),
        Resolved::Missing => None,
    };
    let state_dir = socket::resolve_state_dir().map_err(|e| e.to_string());
    let state_writable = match &state_dir {
        Ok(dir) => writable(dir),
        Err(e) => Err(e.clone()),
    };
    let socket = state_dir.as_ref().ok().map(|dir| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        socket::socket_paths(dir, &socket::project_root(&cwd), socket::own_uid())
    });
    let git = Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    Facts {
        marion,
        supervisor,
        supervisor_version,
        os: std::env::consts::OS.to_string(),
        state_dir,
        state_writable,
        socket,
        git,
    }
}

/// The findings, in the order a person should fix them.
pub fn checks(f: &Facts) -> Vec<Check> {
    vec![
        binaries(f),
        os(&f.os),
        state(f),
        socket_fit(f),
        match &f.git {
            Some(v) => Check::new(Level::Ok, v.clone()),
            None => Check::new(
                Level::Warn,
                "git not found: children cannot run with worktree isolation (each child's own \
                 branch marion/<id>), and a root with file tools needs a git repository",
            ),
        },
    ]
}

/// The version word out of `<name> <version>`.
fn version_word(output: &str) -> &str {
    output.split_whitespace().nth(1).unwrap_or(output)
}

fn binaries(f: &Facts) -> Check {
    let Some((marion, marion_out)) = &f.marion else {
        return Check::new(
            Level::Warn,
            format!(
                "marion not found beside this supervisor or on $PATH; this is marion-supervisor {}",
                env!("CARGO_PKG_VERSION")
            ),
        );
    };
    let (supervisor, how) = match &f.supervisor {
        Resolved::Beside(p) => (p, "beside marion"),
        Resolved::OnPath(p) => (p, "from $PATH, not beside marion"),
        Resolved::Missing => {
            return Check::new(
                Level::Fail,
                format!(
                    "marion ({}) cannot start a supervisor: no marion-supervisor beside it or on \
                     $PATH. Install both binaries into the same directory",
                    marion.display()
                ),
            );
        }
    };
    let (Some(m), Some(s)) = (marion_out.as_deref(), f.supervisor_version.as_deref()) else {
        return Check::new(
            Level::Fail,
            format!(
                "could not read the version of {} or {}",
                marion.display(),
                supervisor.display()
            ),
        );
    };
    let (mv, sv) = (version_word(m), version_word(s));
    let text = format!(
        "marion {mv} ({}) and marion-supervisor {sv} ({}, {how})",
        marion.display(),
        supervisor.display()
    );
    if mv != sv {
        Check::new(
            Level::Fail,
            format!("{text}: the two versions differ; reinstall both into the same directory"),
        )
    } else {
        Check::new(Level::Ok, text)
    }
}

fn os(os: &str) -> Check {
    match os {
        "macos" => Check::new(Level::Ok, "macOS"),
        "linux" => Check::new(
            Level::Warn,
            "Linux: headless runs work, but the native lanes (`marion <harness>`) are not yet \
             verified on Linux",
        ),
        other => Check::new(
            Level::Fail,
            format!("{other} is not supported: marion runs on macOS and Linux"),
        ),
    }
}

fn state(f: &Facts) -> Check {
    match (&f.state_dir, &f.state_writable) {
        (Ok(dir), Ok(())) => Check::new(
            Level::Ok,
            format!("state dir {} is writable", dir.display()),
        ),
        (Ok(dir), Err(why)) => Check::new(
            Level::Fail,
            format!(
                "state dir {} is not writable ({why}); set MARION_STATE_DIR to a directory you own",
                dir.display()
            ),
        ),
        (Err(why), _) => Check::new(Level::Fail, why.clone()),
    }
}

fn socket_fit(f: &Facts) -> Check {
    let Some(paths) = &f.socket else {
        return Check::new(Level::Warn, "socket path not checked: no state dir");
    };
    let len = paths.socket().as_os_str().len();
    let limit = format!(
        "{} bytes, limit {} (sun_path is 104 on macOS, 108 on Linux; marion keeps to 104)",
        len,
        socket::MAX_SOCKET_PATH_BYTES
    );
    match paths.overflow() {
        None => Check::new(
            Level::Ok,
            format!("socket {} fits: {limit}", paths.socket().display()),
        ),
        Some(_) => Check::new(
            Level::Warn,
            format!(
                "the state dir is too long for a unix socket, so this project's socket is {} \
                 instead ({limit}); a shorter MARION_STATE_DIR keeps it with the rest of the state",
                paths.socket().display()
            ),
        ),
    }
}

/// `marion` as `marion doctor` named it, else beside this binary, else on `$PATH`.
fn find_marion() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(CLIENT_EXE_ENV).map(PathBuf::from) {
        return Some(p);
    }
    let beside = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("marion"))
        .filter(|p| p.is_file());
    beside.or_else(|| on_path("marion"))
}

/// marion's own rule for the supervisor it starts: the one beside it, else `$PATH`'s.
fn resolve_supervisor(marion: &Path) -> Resolved {
    let beside = marion.with_file_name("marion-supervisor");
    if beside.is_file() {
        return Resolved::Beside(beside);
    }
    on_path("marion-supervisor").map_or(Resolved::Missing, Resolved::OnPath)
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn same_file(a: Option<&Path>, b: Option<&Path>) -> bool {
    match (
        a.and_then(|p| p.canonicalize().ok()),
        b.and_then(|p| p.canonicalize().ok()),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn version_of(program: &Path) -> Option<String> {
    Command::new(program)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Create the directory as any verb would, then prove a file can be written in it.
fn writable(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let probe = dir.join(format!(".doctor-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            marion: Some((PathBuf::from("/bin/marion"), Some("marion 0.1.0".into()))),
            supervisor: Resolved::Beside(PathBuf::from("/bin/marion-supervisor")),
            supervisor_version: Some("marion-supervisor 0.1.0".into()),
            os: "macos".into(),
            state_dir: Ok(PathBuf::from("/s")),
            state_writable: Ok(()),
            socket: Some(socket::socket_paths(
                Path::new("/s"),
                Path::new("/p/.git"),
                501,
            )),
            git: Some("git version 2.50.0".into()),
        }
    }

    fn levels(f: &Facts) -> Vec<Level> {
        checks(f).iter().map(|c| c.level).collect()
    }

    /// **A healthy machine is all ok**, and the binaries line names both paths, both versions
    /// and how the supervisor was found.
    #[test]
    fn a_healthy_machine_reports_every_check_ok() {
        let f = facts();
        assert!(
            levels(&f).iter().all(|l| *l == Level::Ok),
            "{:?}",
            checks(&f)
        );
        let bin = &checks(&f)[0].text;
        assert!(
            bin.contains("marion 0.1.0 (/bin/marion)")
                && bin.contains("marion-supervisor 0.1.0 (/bin/marion-supervisor, beside marion)"),
            "{bin}"
        );
        assert!(checks(&f)[3].text.contains("fits"), "{:?}", checks(&f)[3]);
    }

    /// **Two binaries of different builds fail**: they speak one wire and must ship as a pair.
    /// A supervisor found only on `$PATH` is said, and is fine when the versions match.
    #[test]
    fn mismatched_or_missing_binaries_fail_and_a_path_supervisor_is_named() {
        let mut f = facts();
        f.supervisor_version = Some("marion-supervisor 0.2.0".into());
        assert_eq!(checks(&f)[0].level, Level::Fail, "{:?}", checks(&f)[0]);
        assert!(checks(&f)[0].text.contains("differ"));

        let mut f = facts();
        f.supervisor = Resolved::OnPath(PathBuf::from("/usr/bin/marion-supervisor"));
        assert_eq!(checks(&f)[0].level, Level::Ok);
        assert!(
            checks(&f)[0].text.contains("from $PATH"),
            "{:?}",
            checks(&f)[0]
        );

        let mut f = facts();
        f.supervisor = Resolved::Missing;
        f.supervisor_version = None;
        assert_eq!(checks(&f)[0].level, Level::Fail);
    }

    /// **Linux warns, an unsupported OS fails, a missing git warns naming worktree isolation, and
    /// an unwritable state dir fails** — each naming what to do.
    #[test]
    fn each_environment_problem_has_its_level() {
        let mut f = facts();
        f.os = "linux".into();
        assert_eq!(levels(&f)[1], Level::Warn);
        assert!(checks(&f)[1].text.contains("native lanes"));
        f.os = "windows".into();
        assert_eq!(levels(&f)[1], Level::Fail);

        let mut f = facts();
        f.git = None;
        assert_eq!(levels(&f)[4], Level::Warn);
        assert!(checks(&f)[4].text.contains("worktree isolation"));

        let mut f = facts();
        f.state_writable = Err("Permission denied".into());
        assert_eq!(levels(&f)[2], Level::Fail);
        assert!(checks(&f)[2].text.contains("MARION_STATE_DIR"));
    }

    /// **A state dir too long for a socket warns and names where the socket went**, since marion
    /// falls back rather than failing.
    #[test]
    fn a_state_dir_too_long_for_a_socket_warns_with_the_fallback() {
        let mut f = facts();
        let long = PathBuf::from(format!("/{}", "d".repeat(120)));
        f.socket = Some(socket::socket_paths(&long, Path::new("/p/.git"), 501));
        let c = &checks(&f)[3];
        assert_eq!(c.level, Level::Warn, "{c:?}");
        assert!(c.text.contains("/tmp/marion-501/"), "{c:?}");
    }
}
