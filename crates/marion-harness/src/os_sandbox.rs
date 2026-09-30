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
//! The sandbox applies only where a node's harness home is marion's own — canned and endpoint
//! auth, where the home lives in the agent dir and marion owns the environment. **On the
//! operator's own login a node runs as its harness normally does**, in the harness's own
//! permission or auto mode, with no marion sandbox layered on ([`ON_OWN_LOGIN`]); its containment
//! is its row's own (codex's sandbox), as before.

/// **The policy on the operator's own login**, as `marion doctor` and the guide state it: the
/// harness's own mode governs. marion owns neither that home nor that login, so it adds nothing.
pub const ON_OWN_LOGIN: &str = "on your own login, each harness runs in its own permission mode; \
                                marion's sandbox applies to canned and endpoint nodes";

/// How marion's sandbox meets one row's harness. Row data, stated by every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsSandboxRule {
    /// The harness has no sandbox of its own on by default: marion's profile wraps its process.
    /// `writes` are the paths beyond the node's own that it measured it needs.
    Wrap { writes: &'static [WritePath] },
    /// The harness's own sandbox cannot run inside marion's, so it is switched off where marion's
    /// applies: `off` is `(field, JSON value)` on the thread's opening request, which beats every
    /// other switch the harness reads. A read-only node keeps the harness's own read-only sandbox
    /// instead, which needs no replacing.
    ReplaceOwn {
        writes: &'static [WritePath],
        off: &'static [(&'static str, &'static str)],
    },
    /// Not applied, and why: the node keeps the containment its row had without it.
    Unsupported { why: &'static str },
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
    matches!(auth, crate::Auth::Canned | crate::Auth::Endpoint)
        && match rule {
            OsSandboxRule::Wrap { .. } => true,
            OsSandboxRule::ReplaceOwn { .. } => !read_only,
            OsSandboxRule::Unsupported { .. } => false,
        }
}

/// Whether it applies to **this launch**: asked for (the supervisor's node launches set
/// [`crate::adapter::Extras::os_sandbox`]; a probe does not), [`covers`], and a host that
/// supports it.
pub fn applies(rule: OsSandboxRule, spec: &crate::adapter::LaunchSpec) -> bool {
    spec.extra.os_sandbox && covers(rule, spec.auth, spec.extra.read_only) && support().available()
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
        let writes: &[WritePath] = match rule {
            OsSandboxRule::Wrap { writes } | OsSandboxRule::ReplaceOwn { writes, .. } => writes,
            OsSandboxRule::Unsupported { why } => return Err(why.to_string()),
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
        for w in writes {
            let path = match w {
                WritePath::Home(rel) => home.join(rel),
                WritePath::HomeProject(rel) => home.join(rel).join(project_key(&cwd)),
                WritePath::TmpUserProject(prefix) => PathBuf::from("/tmp")
                    .join(format!("{prefix}-{}", own_uid()))
                    .join(project_key(&cwd)),
            };
            std::fs::create_dir_all(&path)
                .map_err(|e| format!("could not create {}: {e}", path.display()))?;
            dirs.push(canonical(&path)?);
        }
        dirs.dedup();
        Ok(SandboxPlan { dirs })
    }

    /// The directories, in order.
    pub fn dirs(&self) -> &[std::path::PathBuf] {
        &self.dirs
    }

    /// **The Seatbelt profile for `n` writable directories.** Writes are denied except to those
    /// and to the terminal and null devices; everything else — reads, exec, the network, the
    /// supervisor's socket — is left as it was. Paths arrive only as parameters (`W0`…), never in
    /// the profile's text, so no path can change what the profile says.
    pub fn seatbelt_profile(n: usize) -> String {
        let mut p = String::from(
            "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n  \
             (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\")\n  \
             (literal \"/dev/ptmx\") (regex #\"^/dev/ttys[0-9]+$\") \
             (literal \"/dev/dtracehelper\"))\n",
        );
        for i in 0..n {
            p.push_str(&format!("(allow file-write* (subpath (param \"W{i}\")))\n"));
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
        platform_command(&dirs, program, args)
    }
}

#[cfg(target_os = "macos")]
fn platform_command(
    dirs: &[std::path::PathBuf],
    program: &str,
    args: &[String],
) -> std::process::Command {
    let mut cmd = std::process::Command::new(SANDBOX_EXEC);
    cmd.arg("-p").arg(SandboxPlan::seatbelt_profile(dirs.len()));
    for (i, d) in dirs.iter().enumerate() {
        let mut kv = std::ffi::OsString::from(format!("W{i}="));
        kv.push(d.as_os_str());
        cmd.arg("-D").arg(kv);
    }
    cmd.arg(program).args(args);
    cmd
}

#[cfg(target_os = "linux")]
fn platform_command(
    dirs: &[std::path::PathBuf],
    program: &str,
    args: &[String],
) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    landlock::restrict_on_exec(&mut cmd, dirs);
    cmd
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_command(
    _dirs: &[std::path::PathBuf],
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
    pub(super) fn restrict_on_exec(cmd: &mut std::process::Command, dirs: &[std::path::PathBuf]) {
        let handled = handled(abi().unwrap_or(0));
        let file_rights = handled & (WRITE_FILE | TRUNCATE);
        let mut rules: Vec<(CString, u64)> = dirs
            .iter()
            .filter_map(|d| CString::new(d.as_os_str().as_bytes()).ok())
            .map(|c| (c, handled))
            .collect();
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
        let wrap = OsSandboxRule::Wrap { writes: &[] };
        let replace = OsSandboxRule::ReplaceOwn {
            writes: &[],
            off: &[],
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
        let p = SandboxPlan::seatbelt_profile(3);
        assert!(p.contains("(deny file-write*)"));
        for i in 0..3 {
            assert!(p.contains(&format!("(subpath (param \"W{i}\"))")), "{p}");
        }
        assert!(!p.contains("W3"));
        assert!(!p.contains("/Users") && !p.contains("/home"), "{p}");
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
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
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
                OsSandboxRule::Wrap { writes } | OsSandboxRule::ReplaceOwn { writes, .. } => writes,
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
