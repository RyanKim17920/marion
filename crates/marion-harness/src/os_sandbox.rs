//! **marion's own OS sandbox around a node**: writes only to the node's workspace, its `TMPDIR`
//! and its agent dir, plus the few paths its row measured it needs — macOS Seatbelt through
//! `sandbox-exec`, Linux Landlock at ABI 2 or later. Reads stay broad.
//!
//! Every row states how the sandbox meets its harness ([`OsSandboxRule`]): marion **wraps** a
//! harness with no sandbox of its own on by default; it **replaces** one whose own sandbox cannot
//! run inside marion's (the kernel refuses a nested Seatbelt, measured on codex); and a row nobody
//! has measured under the profile is **unsupported**, so it keeps the containment it had. Nothing
//! here is per harness: a row's strategy and its extra write paths are data.
//!
//! Canned and endpoint nodes are covered wherever their row states a strategy: their harness home
//! lives in the agent dir. Under the operator's own login a harness writes its real home, and a
//! writable home is a way out of the sandbox (a hook the operator's next unsandboxed session
//! runs), so a row is covered there only once its [`Live`] list — session and state paths, and
//! single files such as a credential a login refresh rewrites, never settings or hooks — has
//! passed an admission run: one live task under the profile, with nothing written outside it.

/// How marion's sandbox meets one row's harness. Row data, stated by every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsSandboxRule {
    /// The harness has no sandbox of its own on by default: marion's profile wraps its process.
    /// `writes` are the paths beyond the node's own that it measured it needs; `live` what it
    /// needs more on the operator's own login.
    Wrap {
        writes: &'static [WritePath],
        live: Live,
    },
    /// The harness's own sandbox cannot run inside marion's, so it is switched off where marion's
    /// applies: `off` is `(field, JSON value)` on the thread's opening request, which beats every
    /// other switch the harness reads. A read-only node keeps the harness's own read-only sandbox
    /// instead, which needs no replacing.
    ReplaceOwn {
        writes: &'static [WritePath],
        off: &'static [(&'static str, &'static str)],
        live: Live,
    },
    /// Not applied, and why: the node keeps the containment its row had without it.
    Unsupported { why: &'static str },
}

/// **What a row's harness writes on the operator's own login**, beyond what it writes anywhere:
/// its session and state directories, and single files it rewrites (a credential a login refresh
/// replaces, a history it appends to) — never its settings, hooks, agents or skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    pub writes: &'static [WritePath],
    /// Files relative to the operator's home, each writable alone — never its directory.
    pub files: &'static [&'static str],
    /// The admission run that showed these are enough and nothing else was written — harness
    /// version, date, task. `None` until one passes: the row is then not covered on the
    /// operator's own login, and its nodes keep the containment their row has without it.
    pub admitted: Option<&'static str>,
}

/// A row with nothing measured for the operator's own login.
pub const UNMEASURED: Live = Live {
    writes: &[],
    files: &[],
    admitted: None,
};

impl OsSandboxRule {
    /// The row's list for the operator's own login, where it states a strategy.
    pub fn live(self) -> Option<Live> {
        match self {
            OsSandboxRule::Wrap { live, .. } | OsSandboxRule::ReplaceOwn { live, .. } => Some(live),
            OsSandboxRule::Unsupported { .. } => None,
        }
    }
}

/// The variable an operator sets to `1` for an **admission run**: a live node on a row whose
/// [`Live`] list is stated but not yet admitted runs under the profile, so the run can show the
/// list is enough. It only ever adds a sandbox; the node's containment is judged as before.
pub const ADMIT_ENV: &str = "MARION_SANDBOX_ADMIT";

fn admitting() -> bool {
    std::env::var(ADMIT_ENV).is_ok_and(|v| v.trim() == "1")
}

/// A path a harness writes beyond its node's own dirs, relative to the **node's** home: the
/// `HOME` its launch sets where it sets one (a canned home in the agent dir), else the operator's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePath {
    /// `$HOME/<rel>`.
    Home(&'static str),
    /// `$HOME/<rel>/<the node's cwd, every byte not ASCII alphanumeric as '-'>` — one project's
    /// directory under a harness home keyed on the working directory (claude's `projects/`).
    HomeProject(&'static str),
    /// `/tmp/<prefix>-<uid>/<the node's cwd, keyed as for HomeProject>` — one project's directory
    /// under a harness's per-user scratch root in `/tmp` (claude's Bash tool keeps its working
    /// state there, not under `$TMPDIR`).
    TmpUserProject(&'static str),
}

impl WritePath {
    /// Where this path is for a node whose home is `home` and whose working directory is `cwd`.
    pub fn resolve(self, home: &std::path::Path, cwd: &std::path::Path) -> std::path::PathBuf {
        match self {
            WritePath::Home(rel) => home.join(rel),
            WritePath::HomeProject(rel) => home.join(rel).join(project_key(cwd)),
            WritePath::TmpUserProject(prefix) => std::path::PathBuf::from("/tmp")
                .join(format!("{prefix}-{}", own_uid()))
                .join(project_key(cwd)),
        }
    }
}

/// **A row's own paths on the operator's own login**, resolved against `home` and `cwd` without
/// touching the disk: the directories (its paths for every auth, then its [`Live`] ones) and its
/// single files. What an admission run checks its `$HOME` diff against.
pub fn live_paths(
    rule: OsSandboxRule,
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let (writes, live): (&[WritePath], Live) = match rule {
        OsSandboxRule::Wrap { writes, live } | OsSandboxRule::ReplaceOwn { writes, live, .. } => {
            (writes, live)
        }
        OsSandboxRule::Unsupported { .. } => return (Vec::new(), Vec::new()),
    };
    let dirs = writes
        .iter()
        .chain(live.writes)
        .map(|w| w.resolve(home, cwd))
        .collect();
    let files = live.files.iter().map(|f| home.join(f)).collect();
    (dirs, files)
}

/// What this host offers marion's sandbox, probed once per process ([`support`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    /// macOS Seatbelt, applied by `sandbox-exec`.
    Seatbelt,
    /// Linux Landlock at this ABI: 2 or later, because without `REFER` every rename across
    /// directories is refused, and git renames its object files into place.
    Landlock { abi: u32 },
    /// Neither, and why. Nodes then run as before, under the containment their rows had without
    /// the sandbox, and `marion doctor` says so.
    Unavailable(String),
}

impl Support {
    pub fn available(&self) -> bool {
        !matches!(self, Support::Unavailable(_))
    }

    /// One line for `marion doctor`.
    pub fn describe(&self) -> String {
        match self {
            Support::Seatbelt => "macOS Seatbelt (sandbox-exec)".into(),
            Support::Landlock { abi } => format!("Linux Landlock ABI {abi}"),
            Support::Unavailable(why) => format!("unavailable: {why}"),
        }
    }
}

/// The program that applies a Seatbelt profile and `exec`s its command, so the pid marion holds
/// is the harness's own.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// **Whether this host can apply marion's sandbox**, probed once: an empty profile applied for
/// real on macOS (which fails inside another sandbox, since the kernel will not nest one), the
/// Landlock ABI on Linux.
pub fn support() -> &'static Support {
    static SUPPORT: std::sync::OnceLock<Support> = std::sync::OnceLock::new();
    SUPPORT.get_or_init(probe)
}

#[cfg(target_os = "macos")]
fn probe() -> Support {
    use std::process::{Command, Stdio};
    match Command::new(SANDBOX_EXEC)
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(s) if s.success() => Support::Seatbelt,
        Ok(s) => Support::Unavailable(format!(
            "{SANDBOX_EXEC} could not apply even an empty profile ({s}); marion is most likely \
             running inside a sandbox already, and macOS will not nest one"
        )),
        Err(e) => Support::Unavailable(format!("{SANDBOX_EXEC} could not run: {e}")),
    }
}

#[cfg(target_os = "linux")]
fn probe() -> Support {
    match landlock::abi() {
        Some(abi) if abi >= 2 => Support::Landlock { abi },
        Some(abi) => Support::Unavailable(format!(
            "this kernel's Landlock is ABI {abi}; marion needs 2 or later, whose `REFER` lets a \
             node rename files across its own directories"
        )),
        None => Support::Unavailable(
            "this kernel has no Landlock, or it is disabled (`lsm=` on the kernel command line)"
                .into(),
        ),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn probe() -> Support {
    Support::Unavailable("marion's sandbox is built for macOS and Linux only".into())
}

/// The variable an operator sets to `off` to run nodes without marion's sandbox — their
/// containment is then judged without it too, so nothing is labelled contained that is not.
pub const SANDBOX_ENV: &str = "MARION_SANDBOX";

/// Whether the operator leaves marion's sandbox on: anything but `off` in [`SANDBOX_ENV`] is on,
/// so a misspelling never turns it off.
pub fn enabled_by_operator() -> bool {
    std::env::var(SANDBOX_ENV).map_or(true, |v| v.trim() != "off")
}

/// **Whether marion's sandbox bounds a node of this row under `auth`**, host aside: a row that
/// states a strategy, a harness home that is marion's own (canned or endpoint), and — on a row
/// replacing its own sandbox — a node that writes, since a read-only one keeps the harness's.
pub fn covers(rule: OsSandboxRule, auth: crate::Auth, read_only: bool) -> bool {
    let home_ok = match auth {
        crate::Auth::Canned | crate::Auth::Endpoint => true,
        crate::Auth::Inherited => rule.live().is_some_and(|l| l.admitted.is_some()),
    };
    home_ok
        && match rule {
            OsSandboxRule::Wrap { .. } => true,
            OsSandboxRule::ReplaceOwn { .. } => !read_only,
            OsSandboxRule::Unsupported { .. } => false,
        }
}

/// Whether an admission run puts this live launch under the profile: [`ADMIT_ENV`] set, the
/// operator's own login, and a row with a [`Live`] list stated.
fn admits(rule: OsSandboxRule, auth: crate::Auth, read_only: bool, admitting: bool) -> bool {
    admitting
        && auth == crate::Auth::Inherited
        && rule
            .live()
            .is_some_and(|l| !l.writes.is_empty() || !l.files.is_empty())
        && !(matches!(rule, OsSandboxRule::ReplaceOwn { .. }) && read_only)
}

/// Whether it applies to **this launch**: asked for (the supervisor's node launches set
/// [`crate::adapter::Extras::os_sandbox`]; a probe does not), [`covers`], and a host that
/// supports it.
pub fn applies(rule: OsSandboxRule, spec: &crate::adapter::LaunchSpec) -> bool {
    let (auth, read_only) = (spec.auth, spec.extra.read_only);
    spec.extra.os_sandbox
        && (covers(rule, auth, read_only) || admits(rule, auth, read_only, admitting()))
        && support().available()
}

/// The fields a row replacing its own sandbox sets on its thread's opening request where marion's
/// applies — nothing on any other launch.
pub fn replaced_fields(
    rule: OsSandboxRule,
    spec: &crate::adapter::LaunchSpec,
) -> &'static [(&'static str, &'static str)] {
    match rule {
        OsSandboxRule::ReplaceOwn { off, .. } if applies(rule, spec) => off,
        _ => &[],
    }
}

/// **One node's sandbox, resolved**: every directory it may write, canonical — Seatbelt and
/// Landlock both match resolved paths, so `/tmp/x` must be named `/private/tmp/x` on macOS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPlan {
    dirs: Vec<std::path::PathBuf>,
    /// Single files writable alone ([`Live::files`]), on the operator's own login.
    files: Vec<std::path::PathBuf>,
}

impl SandboxPlan {
    /// The plan for a launch of `spec` rendered as `inv` on a row with `rule`: the node's agent
    /// dir (its config, contracts and `TMPDIR` live there), its working directory, the admin dir
    /// of the git worktree it works in, and the row's own paths under the node's home — each made
    /// to exist and resolved. `Err` names what could not be, and refuses the launch: a node marion
    /// labels contained never runs looser than its label.
    pub fn for_launch(
        rule: OsSandboxRule,
        spec: &crate::adapter::LaunchSpec,
        inv: &crate::invocation::Invocation,
    ) -> Result<SandboxPlan, String> {
        use std::path::PathBuf;
        let (writes, live): (&[WritePath], Live) = match rule {
            OsSandboxRule::Wrap { writes, live }
            | OsSandboxRule::ReplaceOwn { writes, live, .. } => (writes, live),
            OsSandboxRule::Unsupported { why } => return Err(why.to_string()),
        };
        let (live_writes, live_files): (&[WritePath], &[&str]) = match spec.auth {
            crate::Auth::Inherited => (live.writes, live.files),
            crate::Auth::Canned | crate::Auth::Endpoint => (&[], &[]),
        };
        let agent_dir = spec.config_dir.parent().ok_or_else(|| {
            format!(
                "the node's config dir {} has no agent dir above it",
                spec.config_dir.display()
            )
        })?;
        let cwd = canonical(&inv.cwd)?;
        let home = inv
            .env
            .iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| PathBuf::from(v))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .ok_or("the node has no HOME to place its row's paths under")?;
        let mut dirs = vec![canonical(agent_dir)?, cwd.clone()];
        if let Some(admin) = worktree_admin_dir(&cwd) {
            dirs.push(canonical(&admin)?);
        }
        for w in writes.iter().chain(live_writes) {
            let path = w.resolve(&home, &cwd);
            std::fs::create_dir_all(&path)
                .map_err(|e| format!("could not create {}: {e}", path.display()))?;
            dirs.push(canonical(&path)?);
        }
        dirs.dedup();
        let mut files = Vec::new();
        for f in live_files {
            let path = home.join(f);
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(format!("{} names no file", path.display()));
            };
            // The file may not exist yet (a login never refreshed); its directory must.
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
            files.push(canonical(parent)?.join(name));
        }
        Ok(SandboxPlan { dirs, files })
    }

    /// The single files, in order.
    pub fn files(&self) -> &[std::path::PathBuf] {
        &self.files
    }

    /// The directories, in order.
    pub fn dirs(&self) -> &[std::path::PathBuf] {
        &self.dirs
    }

    /// **The Seatbelt profile for `n` writable directories.** Writes are denied except to those
    /// and to the terminal and null devices; everything else — reads, exec, the network, the
    /// supervisor's socket — is left as it was. Paths arrive only as parameters (`W0`…), never in
    /// the profile's text, so no path can change what the profile says.
    pub fn seatbelt_profile(n: usize, files: usize) -> String {
        let mut p = String::from(
            "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n  \
             (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\")\n  \
             (literal \"/dev/ptmx\") (regex #\"^/dev/ttys[0-9]+$\") \
             (literal \"/dev/dtracehelper\"))\n",
        );
        for i in 0..n {
            p.push_str(&format!("(allow file-write* (subpath (param \"W{i}\")))\n"));
        }
        for i in 0..files {
            p.push_str(&format!("(allow file-write* (literal (param \"F{i}\")))\n"));
        }
        p
    }

    /// The process that runs `program args` under this plan, with `tmpdir` writable too.
    pub(crate) fn command(
        &self,
        program: &str,
        args: &[String],
        tmpdir: &std::path::Path,
    ) -> std::process::Command {
        let mut dirs = self.dirs.clone();
        // A TMPDIR outside the agent dir (a test's) is still the node's own.
        if let Ok(t) = canonical(tmpdir)
            && !dirs.iter().any(|d| t.starts_with(d))
        {
            dirs.push(t);
        }
        platform_command(&dirs, &self.files, program, args)
    }
}

#[cfg(target_os = "macos")]
fn platform_command(
    dirs: &[std::path::PathBuf],
    files: &[std::path::PathBuf],
    program: &str,
    args: &[String],
) -> std::process::Command {
    let mut cmd = std::process::Command::new(SANDBOX_EXEC);
    cmd.arg("-p")
        .arg(SandboxPlan::seatbelt_profile(dirs.len(), files.len()));
    for (key, paths) in [("W", dirs), ("F", files)] {
        for (i, d) in paths.iter().enumerate() {
            let mut kv = std::ffi::OsString::from(format!("{key}{i}="));
            kv.push(d.as_os_str());
            cmd.arg("-D").arg(kv);
        }
    }
    cmd.arg(program).args(args);
    cmd
}

#[cfg(target_os = "linux")]
fn platform_command(
    dirs: &[std::path::PathBuf],
    files: &[std::path::PathBuf],
    program: &str,
    args: &[String],
) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    landlock::restrict_on_exec(&mut cmd, dirs, files);
    cmd
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_command(
    _dirs: &[std::path::PathBuf],
    _files: &[std::path::PathBuf],
    program: &str,
    args: &[String],
) -> std::process::Command {
    // Unreachable: no plan exists where `support` is unavailable. Fail at exec rather than run a
    // node looser than its label.
    let mut cmd = std::process::Command::new("/nonexistent/marion-sandbox-unsupported");
    cmd.arg(program).args(args);
    cmd
}

/// This process's real uid, as a per-user scratch root in `/tmp` is named.
fn own_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: `getuid` reads the calling process's real uid and cannot fail.
    unsafe { getuid() }
}

/// `path` resolved, or why not.
fn canonical(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    std::fs::canonicalize(path).map_err(|e| format!("could not resolve {}: {e}", path.display()))
}

/// The admin dir of the git worktree at `cwd` (`<common>/worktrees/<name>`, where its index and
/// HEAD live), read from the `.git` file a linked worktree has. `None` for a main checkout, whose
/// `.git` directory is inside `cwd` already. The shared object store and refs stay unwritable:
/// marion makes a child's commit itself, from outside the sandbox.
fn worktree_admin_dir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(cwd.join(".git")).ok()?;
    let dir = text.strip_prefix("gitdir:")?.trim();
    Some(cwd.join(dir))
}

/// `cwd` as a harness keys a project directory on it: every byte not ASCII alphanumeric as `-`.
fn project_key(cwd: &std::path::Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// **Landlock, by its syscalls**: a ruleset handling every write right, a rule per writable
/// directory (and the terminal and null devices), `no_new_privs`, then `restrict_self` — all in
/// the child between `fork` and `exec`, over paths prepared before the fork.
#[cfg(target_os = "linux")]
mod landlock {
    use std::ffi::{CString, c_char, c_int, c_long, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;

    const CREATE_RULESET: c_long = 444;
    const ADD_RULE: c_long = 445;
    const RESTRICT_SELF: c_long = 446;
    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: c_int = 1;
    const PR_SET_NO_NEW_PRIVS: c_int = 38;
    const O_PATH: c_int = 0o10000000;
    const O_CLOEXEC: c_int = 0o2000000;

    const WRITE_FILE: u64 = 1 << 1;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const MAKE_SOCK: u64 = 1 << 9;
    const MAKE_FIFO: u64 = 1 << 10;
    const MAKE_BLOCK: u64 = 1 << 11;
    const MAKE_SYM: u64 = 1 << 12;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;

    unsafe extern "C" {
        fn syscall(number: c_long, ...) -> c_long;
        fn prctl(option: c_int, ...) -> c_int;
        fn open(path: *const c_char, flags: c_int, ...) -> c_int;
        fn close(fd: c_int) -> c_int;
    }

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// The kernel's Landlock ABI, or `None` where it has none.
    pub(super) fn abi() -> Option<u32> {
        // SAFETY: a version query reads no attribute and creates nothing.
        let v = unsafe {
            syscall(
                CREATE_RULESET,
                std::ptr::null::<c_void>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        (v > 0).then_some(v as u32)
    }

    fn handled(abi: u32) -> u64 {
        let base = WRITE_FILE
            | REMOVE_DIR
            | REMOVE_FILE
            | MAKE_CHAR
            | MAKE_DIR
            | MAKE_REG
            | MAKE_SOCK
            | MAKE_FIFO
            | MAKE_BLOCK
            | MAKE_SYM
            | REFER;
        if abi >= 3 { base | TRUNCATE } else { base }
    }

    /// Restrict `cmd`'s child to writing only beneath `dirs` (and the terminal and null devices).
    pub(super) fn restrict_on_exec(
        cmd: &mut std::process::Command,
        dirs: &[std::path::PathBuf],
        files: &[std::path::PathBuf],
    ) {
        let handled = handled(abi().unwrap_or(0));
        let file_rights = handled & (WRITE_FILE | TRUNCATE);
        let mut rules: Vec<(CString, u64)> = dirs
            .iter()
            .filter_map(|d| CString::new(d.as_os_str().as_bytes()).ok())
            .map(|c| (c, handled))
            .collect();
        // A single file is a rule on the file itself, with file rights only: it exists by exec or
        // it is not the node's to create.
        rules.extend(
            files
                .iter()
                .filter_map(|f| CString::new(f.as_os_str().as_bytes()).ok())
                .map(|c| (c, file_rights)),
        );
        for dev in ["/dev/null", "/dev/zero", "/dev/tty", "/dev/ptmx"] {
            rules.push((CString::new(dev).expect("no NUL"), file_rights));
        }
        rules.push((CString::new("/dev/pts").expect("no NUL"), handled));
        // SAFETY: the closure makes only async-signal-safe syscalls over memory prepared before
        // the fork; a failure is the spawn's error, so no node runs unrestricted.
        unsafe {
            cmd.pre_exec(move || {
                let attr = RulesetAttr {
                    handled_access_fs: handled,
                };
                let ruleset = syscall(
                    CREATE_RULESET,
                    &attr as *const RulesetAttr,
                    std::mem::size_of::<RulesetAttr>(),
                    0u32,
                ) as c_int;
                if ruleset < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for (path, rights) in &rules {
                    let fd = open(path.as_ptr(), O_PATH | O_CLOEXEC);
                    if fd < 0 {
                        // A device this host lacks is not one the node needs.
                        continue;
                    }
                    let rule = PathBeneath {
                        allowed_access: *rights,
                        parent_fd: fd,
                    };
                    let rc = syscall(
                        ADD_RULE,
                        ruleset,
                        RULE_PATH_BENEATH,
                        &rule as *const PathBeneath,
                        0u32,
                    );
                    close(fd);
                    if rc != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if prctl(PR_SET_NO_NEW_PRIVS, 1u64, 0u64, 0u64, 0u64) != 0
                    || syscall(RESTRICT_SELF, ruleset, 0u32) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                close(ruleset);
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::harness::Harness;

    use crate::Auth;
    use crate::adapter::{Extras, LaunchSpec, McpDeclaration};
    use crate::invocation::Invocation;

    /// A node's launch in `dir`: its agent dir with a config dir, and a workspace.
    fn node_in(dir: &std::path::Path, auth: Auth) -> (LaunchSpec, Invocation) {
        let agent = dir.join("agent");
        std::fs::create_dir_all(agent.join("config")).unwrap();
        std::fs::create_dir_all(dir.join("wt")).unwrap();
        let spec = LaunchSpec {
            cwd: dir.join("wt"),
            model: None,
            prompt: String::new(),
            tools: vec![],
            allowed_tools: vec![],
            mcp: McpDeclaration::None,
            base_url: None,
            api_key: None,
            auth,
            config_dir: agent.join("config"),
            resume: None,
            wire: None,
            provider: None,
            extra: Extras {
                os_sandbox: true,
                ..Extras::default()
            },
        };
        let inv = Invocation {
            program: "/bin/sh".into(),
            args: vec![],
            env: vec![("HOME".into(), dir.join("home").to_string_lossy().into())],
            env_remove: vec![],
            cwd: dir.join("wt"),
            model: None,
            session_mode: None,
            inherit: None,
            sandbox: None,
        };
        (spec, inv)
    }

    /// **Only where the harness home is marion's own**: canned and endpoint nodes of a row that
    /// states a strategy; never the operator's own login, never an unmeasured row, and never a
    /// read-only node on a row whose own read-only sandbox it keeps.
    #[test]
    fn the_sandbox_covers_a_measured_row_whose_home_is_marions() {
        let wrap = OsSandboxRule::Wrap {
            writes: &[],
            live: UNMEASURED,
        };
        let replace = OsSandboxRule::ReplaceOwn {
            writes: &[],
            off: &[],
            live: UNMEASURED,
        };
        let unsupported = OsSandboxRule::Unsupported { why: "unmeasured" };
        assert!(covers(wrap, Auth::Canned, false));
        assert!(covers(wrap, Auth::Endpoint, true));
        assert!(!covers(wrap, Auth::Inherited, false));
        assert!(covers(replace, Auth::Canned, false));
        assert!(!covers(replace, Auth::Canned, true));
        assert!(!covers(unsupported, Auth::Canned, false));
    }

    /// **The profile names no path**: every writable directory is a parameter, so nothing a path
    /// contains can change what the profile says.
    #[test]
    fn the_seatbelt_profile_names_no_path_only_parameters() {
        let p = SandboxPlan::seatbelt_profile(3, 0);
        assert!(p.contains("(deny file-write*)"));
        for i in 0..3 {
            assert!(p.contains(&format!("(subpath (param \"W{i}\"))")), "{p}");
        }
        assert!(!p.contains("W3"));
        assert!(!p.contains("/Users") && !p.contains("/home"), "{p}");
    }

    const MEASURED: Live = Live {
        writes: &[WritePath::Home(".h/sessions")],
        files: &[".h/auth.json"],
        admitted: None,
    };

    /// **The operator's own login is covered only once a row's list is admitted**; an admission
    /// run puts a stated but unadmitted row under the profile without covering it.
    #[test]
    fn a_live_login_is_covered_only_once_its_list_is_admitted() {
        let stated = OsSandboxRule::Wrap {
            writes: &[],
            live: MEASURED,
        };
        let admitted = OsSandboxRule::Wrap {
            writes: &[],
            live: Live {
                admitted: Some("h 1.0, 2026-10-01: create hello.txt"),
                ..MEASURED
            },
        };
        let unmeasured = OsSandboxRule::Wrap {
            writes: &[],
            live: UNMEASURED,
        };
        assert!(!covers(stated, Auth::Inherited, false));
        assert!(covers(admitted, Auth::Inherited, false));
        assert!(
            covers(stated, Auth::Canned, false),
            "canned needs no admission"
        );
        assert!(admits(stated, Auth::Inherited, false, true));
        assert!(
            !admits(stated, Auth::Inherited, false, false),
            "only when asked"
        );
        assert!(
            !admits(stated, Auth::Canned, false, true),
            "only on the operator's login"
        );
        assert!(
            !admits(unmeasured, Auth::Inherited, false, true),
            "nothing to admit"
        );
    }

    /// **On the operator's own login a plan adds the row's session paths and its single files**,
    /// each file alone; a canned launch of the same row adds neither.
    #[test]
    fn a_live_plan_adds_the_rows_session_paths_and_single_files() {
        let dir = marion_testsupport::scratch("osb-live-plan");
        let (canned, inv) = node_in(&dir, Auth::Canned);
        let live = LaunchSpec {
            auth: Auth::Inherited,
            ..canned.clone()
        };
        let rule = OsSandboxRule::Wrap {
            writes: &[],
            live: MEASURED,
        };
        let c = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        let plan = SandboxPlan::for_launch(rule, &live, &inv).expect("a plan");
        assert!(plan.dirs().contains(&c(&dir.join("home/.h/sessions"))));
        assert_eq!(plan.files(), [c(&dir.join("home/.h")).join("auth.json")]);
        assert!(
            !dir.join("home/.h/auth.json").exists(),
            "a file is allowed, not created"
        );
        let plan = SandboxPlan::for_launch(rule, &canned, &inv).expect("a plan");
        assert!(plan.files().is_empty());
        assert!(!plan.dirs().iter().any(|d| d.ends_with("sessions")));
        let p = SandboxPlan::seatbelt_profile(2, 1);
        assert!(p.contains("(literal (param \"F0\"))"), "{p}");
    }

    /// **A row's live paths resolve without touching the disk**: every-auth paths first, then its
    /// session paths, and its single files apart.
    #[test]
    fn a_rows_live_paths_resolve_against_home_and_cwd() {
        let rule = OsSandboxRule::Wrap {
            writes: &[WritePath::HomeProject(".h/projects")],
            live: MEASURED,
        };
        let (dirs, files) = live_paths(
            rule,
            std::path::Path::new("/home/op"),
            std::path::Path::new("/w/x"),
        );
        assert_eq!(
            dirs,
            [
                std::path::PathBuf::from("/home/op/.h/projects/-w-x"),
                "/home/op/.h/sessions".into()
            ]
        );
        assert_eq!(files, [std::path::PathBuf::from("/home/op/.h/auth.json")]);
    }

    /// **A single file is writable, and its directory is not**: the process may rewrite the file,
    /// and a sibling it tries to create beside it is refused.
    #[test]
    fn a_single_file_is_writable_and_its_directory_is_not() {
        if !support().available() {
            eprintln!("skipped: {}", support().describe());
            return;
        }
        let dir = marion_testsupport::scratch("osb-live-file");
        let (canned, mut inv) = node_in(&dir, Auth::Canned);
        let live = LaunchSpec {
            auth: Auth::Inherited,
            ..canned
        };
        let auth = dir.join("home/.h/auth.json");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(&auth, "old").unwrap();
        let sibling = dir.join("home/.h/other.json");
        inv.args = vec![
            "-c".into(),
            format!(
                "echo new > '{}'; echo x > '{}'",
                auth.display(),
                sibling.display()
            ),
        ];
        let rule = OsSandboxRule::Wrap {
            writes: &[],
            live: MEASURED,
        };
        inv.sandbox = Some(SandboxPlan::for_launch(rule, &live, &inv).unwrap());
        let tmp = dir.join("agent/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        assert_eq!(
            std::fs::read_to_string(&auth).unwrap().trim(),
            "new",
            "{out:?}"
        );
        assert!(!sibling.exists(), "the directory stays unwritable: {out:?}");
    }

    /// **A plan holds the node's own dirs and its row's paths under the node's home**, resolved,
    /// and makes the row's paths exist so a harness finds them.
    #[test]
    fn a_plan_holds_the_node_dirs_and_the_rows_paths_under_its_home() {
        let dir = marion_testsupport::scratch("osb-plan");
        let (spec, inv) = node_in(&dir, Auth::Canned);
        let rule = OsSandboxRule::Wrap {
            writes: &[
                WritePath::Home(".local/state/goose"),
                WritePath::HomeProject(".claude/projects"),
            ],
            live: UNMEASURED,
        };
        let plan = SandboxPlan::for_launch(rule, &spec, &inv).expect("a plan");
        let c = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        let wt = c(&dir.join("wt"));
        assert_eq!(plan.dirs()[0], c(&dir.join("agent")));
        assert_eq!(plan.dirs()[1], wt);
        assert_eq!(plan.dirs()[2], c(&dir.join("home/.local/state/goose")));
        let key: String = wt
            .to_string_lossy()
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect();
        assert_eq!(
            plan.dirs()[3],
            c(&dir.join("home/.claude/projects").join(key))
        );
        assert!(
            SandboxPlan::for_launch(
                OsSandboxRule::Unsupported { why: "unmeasured" },
                &spec,
                &inv
            )
            .is_err()
        );
    }

    /// **Under its plan a process writes its workspace and nothing else**: the write outside is
    /// refused by the kernel and the file never appears.
    #[test]
    fn a_process_under_its_plan_writes_its_workspace_and_nowhere_else() {
        if cfg!(target_os = "macos") {
            assert!(support().available(), "{}", support().describe());
        } else if !support().available() {
            eprintln!("skipped: {}", support().describe());
            return;
        }
        let dir = marion_testsupport::scratch("osb-apply");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let outside = dir.join("outside.txt");
        inv.args = vec![
            "-c".into(),
            format!(
                "echo in > inside.txt; echo out > '{}'; echo tmp > \"$TMPDIR/t.txt\"",
                outside.display()
            ),
        ];
        inv.sandbox = Some(
            SandboxPlan::for_launch(
                OsSandboxRule::Wrap {
                    writes: &[],
                    live: UNMEASURED,
                },
                &spec,
                &inv,
            )
            .unwrap(),
        );
        let tmp = dir.join("agent/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        assert!(dir.join("wt/inside.txt").exists(), "{out:?}");
        assert!(tmp.join("t.txt").exists(), "{out:?}");
        assert!(
            !outside.exists(),
            "the write outside must be refused: {out:?}"
        );
    }

    fn spawn_ctx(dir: &std::path::Path) -> crate::adapter::SpawnCtx {
        crate::adapter::SpawnCtx {
            agent_id: marion_core::contract::AgentId("019f-node".into()),
            agent_type: "codex".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
            repo: dir.join("wt"),
            state_dir: dir.to_path_buf(),
            bridge: "/bin/marion-supervisor".into(),
            bridge_args: vec!["mcp".into()],
        }
    }

    /// **codex's own sandbox is switched off exactly where marion's replaces it**: a writing node
    /// under marion's sandbox opens its thread with `danger-full-access`, since the kernel will
    /// not nest codex's Seatbelt inside marion's; a probe, a live node and a read-only node keep
    /// codex's own.
    #[test]
    fn codex_trades_its_own_sandbox_for_marions_only_where_marions_applies() {
        if !support().available() {
            eprintln!("skipped: {}", support().describe());
            return;
        }
        let dir = marion_testsupport::scratch("osb-codex");
        let (spec, _) = node_in(&dir, Auth::Canned);
        let codex = crate::adapter::adapter_for(Harness::Codex).unwrap();
        let sandbox_of = |spec: &LaunchSpec| {
            codex
                .session_declaration(spec, &spawn_ctx(&dir))
                .unwrap()
                .expect("codex opens a thread")["params"]["sandbox"]
                .clone()
        };
        assert_eq!(sandbox_of(&spec), "danger-full-access");
        let probe = LaunchSpec {
            extra: Extras::default(),
            ..spec.clone()
        };
        assert_eq!(sandbox_of(&probe), "workspace-write");
        let reviewer = LaunchSpec {
            extra: Extras {
                read_only: true,
                ..spec.extra.clone()
            },
            ..spec.clone()
        };
        assert_eq!(sandbox_of(&reviewer), "read-only");
    }

    /// **A node's invocation carries its plan**, so the process runs under it: the one spawn
    /// builder ([`Invocation::command`]) starts `sandbox-exec` where it applies and the harness
    /// itself where it does not.
    #[test]
    fn a_node_compiles_to_its_harness_under_the_sandbox() {
        if !support().available() {
            eprintln!("skipped: {}", support().describe());
            return;
        }
        let dir = marion_testsupport::scratch("osb-compile");
        let (spec, _) = node_in(&dir, Auth::Canned);
        let spec = LaunchSpec {
            base_url: Some("http://127.0.0.1:1/v1".into()),
            model: Some("m".into()),
            ..spec
        };
        let pi = crate::adapter::adapter_for(Harness::Pi).unwrap();
        let inv = pi.compile(&spec, &spawn_ctx(&dir)).expect("compiles");
        assert!(inv.sandbox.is_some(), "a canned pi node is sandboxed");
        if cfg!(target_os = "macos") {
            let cmd = inv.command(&dir.join("agent/tmp"));
            assert_eq!(cmd.get_program(), SANDBOX_EXEC);
        }
        let live = LaunchSpec {
            auth: Auth::Inherited,
            ..spec.clone()
        };
        let inv = pi.compile(&live, &spawn_ctx(&dir)).expect("compiles");
        assert!(
            inv.sandbox.is_none(),
            "a live node keeps its old label, unsandboxed"
        );
    }

    /// **The sweep: every row states how marion's sandbox meets its harness**, and says why where
    /// it does not apply. The rows measured under the profile are wrapped, codex's own sandbox is
    /// replaced, and every extra write path is relative to a home — never an absolute path that
    /// would open the same directory to every operator's nodes alike.
    #[test]
    fn every_row_states_its_sandbox_strategy() {
        for h in Harness::ALL {
            let rule = crate::adapter::harness_spec(h).os_sandbox;
            let writes: &[WritePath] = match rule {
                OsSandboxRule::Wrap { writes, .. } | OsSandboxRule::ReplaceOwn { writes, .. } => {
                    writes
                }
                OsSandboxRule::Unsupported { why } => {
                    assert!(
                        !why.trim().is_empty(),
                        "{h:?} must say why it is not sandboxed"
                    );
                    &[]
                }
            };
            for w in writes {
                let (WritePath::Home(rel)
                | WritePath::HomeProject(rel)
                | WritePath::TmpUserProject(rel)) = w;
                assert!(
                    !rel.starts_with('/') && !rel.contains(".."),
                    "{h:?}: `{rel}` must stay under its root"
                );
                if let WritePath::TmpUserProject(prefix) = w {
                    assert!(
                        !prefix.contains('/'),
                        "{h:?}: `{prefix}` names one directory"
                    );
                }
            }
        }
        let wrapped = |h| {
            matches!(
                crate::adapter::harness_spec(h).os_sandbox,
                OsSandboxRule::Wrap { .. }
            )
        };
        for h in [
            Harness::ClaudeCode,
            Harness::Gemini,
            Harness::OpenCode,
            Harness::Copilot,
            Harness::Goose,
            Harness::Qwen,
            Harness::Pi,
        ] {
            assert!(wrapped(h), "{h:?} was measured under the profile");
        }
        assert!(matches!(
            crate::adapter::harness_spec(Harness::Codex).os_sandbox,
            OsSandboxRule::ReplaceOwn { .. }
        ));
    }
}
