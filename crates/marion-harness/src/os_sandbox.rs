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

/// **One node's sandbox, resolved**: every directory it may write, by a real path — Seatbelt and
/// Landlock both match resolved paths, so `/tmp/x` must be named `/private/tmp/x` on macOS.
///
/// **No component a node may write is resolved.** A node may move any directory it may write
/// and leave a link in its place, the root of a grant included (Seatbelt lets a process rename the
/// root of a `subpath` it may write, measured with `sandbox-exec`), so resolving such a component
/// in a later generation would grant whatever the link named. Each grant is instead its
/// supervisor-owned ancestor resolved (`bases`: the agents dir, `~/.claude/projects`,
/// `/tmp/<prefix>-<uid>`, a checkout's parent) with the node's own components appended as
/// written, each checked to be a real directory. Seatbelt resolves a path when it is used, so a
/// link swapped in later, even by a process of the node still alive, resolves outside the grant
/// and is refused; Landlock opens its rules by walking those same components `O_NOFOLLOW`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPlan {
    dirs: Vec<std::path::PathBuf>,
    /// For each of `dirs`, the resolved ancestor it was built on; what follows it in the dir is
    /// the node's, and is walked without following a link.
    bases: Vec<std::path::PathBuf>,
    /// The directory holding every node's agent dir, resolved: on macOS nothing in another node's
    /// agent dir is readable but its `contracts/` and `events.jsonl` (what a parent's bridge reads
    /// of its children). The rest — its `config/` with its node token, its prompt, its recording —
    /// is that node's. `None` for a plan that is no node's launch.
    siblings: Option<std::path::PathBuf>,
    /// The one tree inside the siblings' dir the node may read, resolved: the parent's worktree it
    /// was spawned over ([`crate::adapter::Extras::parent_tree`]), which its bridge must resolve.
    /// `None` where there is none.
    parent_tree: Option<std::path::PathBuf>,
    /// What the node may not write inside its own writable agent dir, resolved:
    /// [`marion_core::paths::MARION_WRITTEN`] there. Empty for a plan that is no node's launch.
    held: Vec<std::path::PathBuf>,
    /// The node's agent dir as marion names it: a `TMPDIR` below it is the node's through the
    /// agent dir's own grant, never through what it resolves to. `None` for a plan that is no
    /// node's launch.
    agent_dir: Option<std::path::PathBuf>,
}

/// One directory a plan grants: `path`, which is `base` (resolved, no node's to write) with the
/// node's own components appended as written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    base: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl Grant {
    /// Every component of `path` past `base` is a real directory, not a link: a link the node
    /// left there is refused by name rather than planned as a grant that would deny its writes.
    fn check(&self) -> Result<(), String> {
        let mut at = self.base.clone();
        for part in self
            .path
            .strip_prefix(&self.base)
            .unwrap_or(std::path::Path::new(""))
        {
            at.push(part);
            match std::fs::symlink_metadata(&at) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    return Err(format!(
                        "{} is a link or a file where the node's sandbox grants a directory; \
                         refusing to launch with it",
                        at.display()
                    ));
                }
                Err(e) => return Err(format!("could not resolve {}: {e}", at.display())),
            }
        }
        Ok(())
    }
}

/// `path` as a [`Grant`]: its parent resolved, its own name appended as written.
fn below_parent(path: &std::path::Path) -> Result<Grant, String> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return canonical(path).map(|p| Grant {
            base: p.clone(),
            path: p,
        });
    };
    let base = canonical(parent)?;
    Ok(Grant {
        path: base.join(name),
        base,
    })
}

/// **`path` inside the recorded checkout `root`, as a [`Grant`]**: the root as [`below_parent`]
/// makes it, with `path`'s components below the root appended as written. Every directory inside
/// the checkout is the node's to write, so resolving `path`'s parent would follow a link the node
/// left at any level between the root and it (`<root>/a` to an operator directory, for a cwd of
/// `<root>/a/b`); appended literally, [`Grant::check`] refuses that link by name. The root is
/// found as the outermost of `path`'s ancestors resolving to it, so a link above the checkout,
/// which is the operator's, still resolves. A `path` not inside the root, or the root itself, is
/// [`below_parent`]'s.
fn below_checkout(path: &std::path::Path, root: &std::path::Path) -> Result<Grant, String> {
    use std::path::Component;
    let Ok(resolved) = std::fs::canonicalize(root) else {
        return below_parent(path);
    };
    let mut ancestors: Vec<_> = path.ancestors().collect();
    ancestors.reverse();
    let Some(at) = ancestors
        .into_iter()
        .find(|a| std::fs::canonicalize(a).is_ok_and(|c| c == resolved))
    else {
        return below_parent(path);
    };
    let rel = path.strip_prefix(at).unwrap_or(std::path::Path::new(""));
    if rel.as_os_str().is_empty() {
        return below_parent(path);
    }
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!(
            "{} is not a plain path inside the checkout {}; refusing to launch with it",
            path.display(),
            root.display()
        ));
    }
    let base = below_parent(root)?;
    Ok(Grant {
        path: base.path.join(rel),
        base: base.base,
    })
}

/// **No grant holds the agents dir**: `Err` names the first that is it or above it (a checkout with
/// marion's state dir inside it) and says to move the state dir out. Where the sandbox can deny
/// nothing inside a write grant (Landlock), such a grant is every other node's agent dir, its
/// token among it, the node's to read and write.
fn holds_no_siblings(grants: &[Grant], agents: &std::path::Path) -> Result<(), String> {
    match grants.iter().find(|g| agents.starts_with(&g.path)) {
        Some(g) => Err(format!(
            "the node may write {}, which holds marion's state dir {}; this OS sandbox cannot \
             keep other nodes' tokens there from it, so refusing to launch: set \
             MARION_STATE_DIR to a directory outside the checkout",
            g.path.display(),
            agents.display()
        )),
        None => Ok(()),
    }
}

/// The agents dir holding `agent_dir`, as named.
fn agents_lexical(agent_dir: &std::path::Path) -> &std::path::Path {
    agent_dir.parent().unwrap_or(agent_dir)
}

impl SandboxPlan {
    /// The plan for a launch of `spec` rendered as `inv` on a row with `rule`: the node's agent
    /// dir (its config, contracts and `TMPDIR` live there), its working directory, the admin dir
    /// of the git worktree it works in as the supervisor verified it
    /// ([`crate::adapter::Extras::git_admin_dir`]), and the row's own paths under the node's home — each made
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
        let (Some(agents), Some(id)) = (agent_dir.parent(), agent_dir.file_name()) else {
            return Err(format!(
                "the node's agent dir {} has no agents dir above it",
                agent_dir.display()
            ));
        };
        let agents = canonical(agents)?;
        let own = agents.join(id);
        // A path inside the agent dir as the node's own components below the resolved agents
        // dir; one deeper inside the recorded checkout as its components below the checkout's
        // root; anything else as its own name below its resolved parent.
        let grant = |path: &std::path::Path| -> Result<Grant, String> {
            match path.strip_prefix(agent_dir) {
                Ok(rel) => Ok(Grant {
                    base: agents.clone(),
                    path: own.join(rel),
                }),
                Err(_) => match spec.extra.checkout_root.as_deref() {
                    Some(root) => below_checkout(path, root),
                    None => below_parent(path),
                },
            }
        };
        let cwd = grant(&inv.cwd)?;
        let home = inv
            .env
            .iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| PathBuf::from(v))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .ok_or("the node has no HOME to place its row's paths under")?;
        let siblings = Some(agents.clone());
        // Read only, and only to resolve another node's tree: a parent's worktree is named below
        // the resolved agents dir like the node's own. A tree outside the agents dir (a root's
        // checkout) needs no rule, as reads there are not denied; granted, one holding the
        // agents dir would read every sibling's token.
        let parent_tree = match spec.extra.parent_tree.as_deref() {
            Some(pt) => match pt.strip_prefix(agents_lexical(agent_dir)) {
                Ok(rel) => Some(agents.join(rel)),
                Err(_) => Some(canonical(pt)?),
            },
            None => None,
        }
        .filter(|pt| pt.starts_with(&agents) && *pt != agents);
        let held = marion_core::paths::MARION_WRITTEN
            .iter()
            .map(|leaf| own.join(leaf))
            .collect();
        let mut grants = vec![
            Grant {
                base: agents.clone(),
                path: own.clone(),
            },
            cwd.clone(),
        ];
        if let Some(admin) = &spec.extra.git_admin_dir {
            grants.push(below_parent(admin)?);
        }
        for w in writes {
            let path = match w {
                WritePath::Home(rel) => home.join(rel),
                WritePath::HomeProject(rel) => home.join(rel).join(project_key(&cwd.path)),
                WritePath::TmpUserProject(prefix) => PathBuf::from("/tmp")
                    .join(format!("{prefix}-{}", own_uid()))
                    .join(project_key(&cwd.path)),
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
            }
            let g = below_parent(&path)?;
            match std::fs::create_dir(&g.path) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                    return Err(format!("could not create {}: {e}", g.path.display()));
                }
                _ => {}
            }
            grants.push(g);
        }
        for g in &grants {
            g.check()?;
        }
        // Landlock grants whole directories and can deny nothing inside one: a grant holding the
        // agents dir would hand the node every sibling's token, to read and to write.
        if cfg!(target_os = "linux") {
            holds_no_siblings(&grants, &agents)?;
        }
        grants.dedup();
        Ok(SandboxPlan {
            dirs: grants.iter().map(|g| g.path.clone()).collect(),
            bases: grants.into_iter().map(|g| g.base).collect(),
            siblings,
            parent_tree,
            held,
            agent_dir: Some(agent_dir.to_path_buf()),
        })
    }

    /// A plan writing exactly `dirs`, each resolved: for a process marion runs over a node's work
    /// that is not the node's own launch (a test's, above all). `Err` names a dir that could not
    /// be resolved.
    pub fn writing(dirs: &[std::path::PathBuf]) -> Result<SandboxPlan, String> {
        let mut dirs = dirs
            .iter()
            .map(|d| canonical(d))
            .collect::<Result<Vec<_>, _>>()?;
        dirs.dedup();
        Ok(SandboxPlan {
            bases: dirs.clone(),
            dirs,
            siblings: None,
            parent_tree: None,
            held: Vec::new(),
            agent_dir: None,
        })
    }

    /// The directories, in order.
    pub fn dirs(&self) -> &[std::path::PathBuf] {
        &self.dirs
    }

    /// **The Seatbelt profile for `n` writable directories**, and with `siblings` the rules over
    /// other nodes' agent dirs. Writes are denied except to those and to the terminal and null
    /// devices. With `siblings`, which lists the writable directories inside the agents dir `SIB`
    /// (the node's own agent dir, a parent's worktree a `shared-cwd` child works in), nothing inside
    /// `SIB` is written and nothing inside another node's agent dir (`SIB/<id>/…`) is read but its
    /// `contracts/` and `events.jsonl`, and, with `parent_tree`, the one tree `PT` the node was
    /// spawned over; then the listed directories are opened again, read and write. The `held`
    /// paths (`H0`…) inside a writable directory are then refused writes again. Everything else —
    /// reads, exec, the network, the supervisor's socket — is left as it was.
    ///
    /// **In that order, because the last matching rule wins**: a writable directory holding the
    /// agents dir (a checkout with the state dir inside it) is granted first and the sibling rules
    /// override it, so no grant reopens another node's token.
    ///
    /// Paths arrive only as parameters (`W0`…, `SIB`, `PT`, `H0`…), quoted into the one regex by
    /// Seatbelt's own `regex-quote`, never in the profile's text, so no path can change what the
    /// profile says.
    pub fn seatbelt_profile(
        n: usize,
        siblings: Option<&[usize]>,
        parent_tree: bool,
        held: usize,
    ) -> String {
        let mut p = String::from(
            "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n  \
             (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\")\n  \
             (literal \"/dev/ptmx\") (regex #\"^/dev/ttys[0-9]+$\") \
             (literal \"/dev/dtracehelper\"))\n",
        );
        for i in 0..n {
            p.push_str(&format!("(allow file-write* (subpath (param \"W{i}\")))\n"));
        }
        if let Some(inside) = siblings {
            p.push_str(
                "(deny file-write* (regex (string-append \"^\" (regex-quote (param \"SIB\")) \
                 \"/.\")))\n(deny file-read* (regex (string-append \"^\" (regex-quote (param \"SIB\")) \
                 \"/[^/]+/.\")))\n(allow file-read* (regex (string-append \"^\" \
                 (regex-quote (param \"SIB\")) \"/[^/]+/(contracts(/.*)?|events\\\\.jsonl)$\")))\n",
            );
            if parent_tree {
                p.push_str("(allow file-read* (subpath (param \"PT\")))\n");
            }
            for i in inside.iter().filter(|i| **i < n) {
                p.push_str(&format!(
                    "(allow file-read* file-write* (subpath (param \"W{i}\")))\n"
                ));
            }
        }
        // After the allows: the last matching rule wins.
        for i in 0..held {
            p.push_str(&format!("(deny file-write* (subpath (param \"H{i}\")))\n"));
        }
        p
    }

    /// The process that runs `program args` under this plan, with `tmpdir` writable too: a node's
    /// harness ([`crate::invocation::Invocation::command`]), or a verification line run over its
    /// work.
    pub fn command(
        &self,
        program: &str,
        args: &[String],
        tmpdir: &std::path::Path,
    ) -> std::process::Command {
        let mut dirs = self.dirs.clone();
        let mut bases = self.bases.clone();
        // A TMPDIR outside the agent dir (a test's) is still the node's own. One inside it is
        // granted only as part of the agent dir, so a link the node left there grants nothing.
        let in_agent_dir = self
            .agent_dir
            .as_ref()
            .is_some_and(|a| tmpdir.starts_with(a));
        if !in_agent_dir
            && let Ok(t) = canonical(tmpdir)
            && !dirs.iter().any(|d| t.starts_with(d))
        {
            dirs.push(t.clone());
            bases.push(t);
        }
        platform_command(self, &dirs, &bases, program, args)
    }
}

#[cfg(target_os = "macos")]
fn platform_command(
    plan: &SandboxPlan,
    dirs: &[std::path::PathBuf],
    _bases: &[std::path::PathBuf],
    program: &str,
    args: &[String],
) -> std::process::Command {
    let (siblings, parent_tree) = (plan.siblings.as_deref(), plan.parent_tree.as_deref());
    let mut cmd = std::process::Command::new(SANDBOX_EXEC);
    // The writable directories inside the agents dir: opened again after the sibling rules.
    let inside: Vec<usize> = siblings
        .map(|sib| {
            dirs.iter()
                .enumerate()
                .filter(|(_, d)| d.starts_with(sib) && d.as_path() != sib)
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default();
    cmd.arg("-p").arg(SandboxPlan::seatbelt_profile(
        dirs.len(),
        siblings.map(|_| inside.as_slice()),
        parent_tree.is_some(),
        plan.held.len(),
    ));
    let param = |name: &str, d: &std::path::Path| {
        let mut kv = std::ffi::OsString::from(format!("{name}="));
        kv.push(d.as_os_str());
        kv
    };
    if let Some(sib) = siblings {
        cmd.arg("-D").arg(param("SIB", sib));
        if let Some(pt) = parent_tree {
            cmd.arg("-D").arg(param("PT", pt));
        }
    }
    for (i, d) in dirs.iter().enumerate() {
        cmd.arg("-D").arg(param(&format!("W{i}"), d));
    }
    for (i, h) in plan.held.iter().enumerate() {
        cmd.arg("-D").arg(param(&format!("H{i}"), h));
    }
    cmd.arg(program).args(args);
    cmd
}

#[cfg(target_os = "linux")]
fn platform_command(
    // Landlock here grants whole directories and restricts writes only: reads of other nodes'
    // agent dirs are not refused on Linux, nor are writes to the node's own `held` entries,
    // which docs/os-sandbox.md states.
    _plan: &SandboxPlan,
    dirs: &[std::path::PathBuf],
    bases: &[std::path::PathBuf],
    program: &str,
    args: &[String],
) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    landlock::restrict_on_exec(&mut cmd, dirs, bases);
    cmd
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_command(
    _plan: &SandboxPlan,
    _dirs: &[std::path::PathBuf],
    _bases: &[std::path::PathBuf],
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
    // From libc, never spelled here: the numbers differ by architecture (aarch64's O_DIRECTORY
    // and O_NOFOLLOW are x86_64's O_DIRECT and O_LARGEFILE, which O_PATH ignores).
    use libc::{O_CLOEXEC, O_DIRECTORY, O_NOFOLLOW, O_PATH};

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
        fn openat(dirfd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
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

    /// One rule: `base` opened as it resolves, then each of `tail` below it `O_NOFOLLOW`.
    pub(super) struct Rule {
        base: CString,
        tail: Vec<CString>,
        rights: u64,
        /// A device this host may lack: its rule is skipped where it cannot be opened.
        optional: bool,
    }

    /// The rules for `dirs`, each split at its resolved `bases` entry, and the terminal and null
    /// devices. `None` where a path holds a NUL.
    pub(super) fn rules(
        dirs: &[std::path::PathBuf],
        bases: &[std::path::PathBuf],
        handled: u64,
    ) -> Option<Vec<Rule>> {
        let c = |p: &std::ffi::OsStr| CString::new(p.as_bytes()).ok();
        let mut rules = Vec::new();
        for (dir, base) in dirs.iter().zip(bases) {
            let tail = dir.strip_prefix(base).ok()?;
            rules.push(Rule {
                base: c(base.as_os_str())?,
                tail: tail.iter().map(c).collect::<Option<_>>()?,
                rights: handled,
                optional: false,
            });
        }
        let file_rights = handled & (WRITE_FILE | TRUNCATE);
        for (dev, rights) in [
            ("/dev/null", file_rights),
            ("/dev/zero", file_rights),
            ("/dev/tty", file_rights),
            ("/dev/ptmx", file_rights),
            ("/dev/pts", handled),
        ] {
            rules.push(Rule {
                base: CString::new(dev).expect("no NUL"),
                tail: Vec::new(),
                rights,
                optional: true,
            });
        }
        Some(rules)
    }

    /// `rule`'s directory, opened `O_PATH`: its base as it resolves, then each component of its
    /// tail `O_NOFOLLOW | O_DIRECTORY` below the one before, so a link the node left there fails
    /// the open (`ENOTDIR`) rather than adding a rule for what it names. Async-signal-safe.
    ///
    /// # Safety
    /// Only `open`, `openat` and `close` on memory `rule` owns.
    pub(super) unsafe fn open_rule(rule: &Rule) -> c_int {
        unsafe {
            let mut fd = open(rule.base.as_ptr(), O_PATH | O_CLOEXEC);
            for part in &rule.tail {
                if fd < 0 {
                    break;
                }
                let next = openat(
                    fd,
                    part.as_ptr(),
                    O_PATH | O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC,
                );
                close(fd);
                fd = next;
            }
            fd
        }
    }

    /// Restrict `cmd`'s child to writing only beneath `dirs` (and the terminal and null devices).
    /// Each dir's components past its `bases` entry are opened without following a link
    /// ([`open_rule`]); one that cannot be fails the spawn, so no node runs looser than its plan.
    pub(super) fn restrict_on_exec(
        cmd: &mut std::process::Command,
        dirs: &[std::path::PathBuf],
        bases: &[std::path::PathBuf],
    ) {
        let handled = handled(abi().unwrap_or(0));
        let rules = rules(dirs, bases, handled);
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
                let Some(rules) = &rules else {
                    return Err(std::io::Error::from_raw_os_error(22));
                };
                for r in rules {
                    let fd = open_rule(r);
                    if fd < 0 {
                        if r.optional {
                            // A device this host lacks is not one the node needs.
                            continue;
                        }
                        return Err(std::io::Error::last_os_error());
                    }
                    let rule = PathBeneath {
                        allowed_access: r.rights,
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
    /// Where an agent dir holds its node token, as these tests deny it — pinned to the path every
    /// launch writes by [`the_token_path_these_tests_deny_is_the_one_every_launch_writes`].
    const TOKEN_PATH: &str = "config/node-token";

    /// **The sandbox tests deny the token's real path.** A token moved to another file would
    /// otherwise leave every sibling-read test guarding a file nothing writes.
    #[test]
    fn the_token_path_these_tests_deny_is_the_one_every_launch_writes() {
        let dir = marion_testsupport::scratch("osb-token-path");
        let (spec, _) = node_in(&dir, Auth::Canned);
        assert_eq!(
            crate::adapter::token_file(&spec),
            dir.join("agent").join(TOKEN_PATH)
        );
    }

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

    /// **No row's rule sandboxes the operator's own login**, writing or read-only, so a live node
    /// never runs under a profile that would hide its harness home from it. Every row that does
    /// state a strategy still covers a canned node, so the sweep is not vacuous.
    #[test]
    fn no_rows_rule_covers_an_inherited_login() {
        let dir = marion_testsupport::scratch("osb-inherited");
        let (live, _) = node_in(&dir, Auth::Inherited);
        let mut measured = 0;
        for row in &crate::adapter::ROWS {
            let (h, rule) = (row.spec.harness, row.spec.os_sandbox);
            for read_only in [false, true] {
                assert!(
                    !covers(rule, Auth::Inherited, read_only),
                    "{h:?} (read_only {read_only}) sandboxes an inherited login"
                );
                let spec = LaunchSpec {
                    extra: Extras {
                        read_only,
                        ..live.extra.clone()
                    },
                    ..live.clone()
                };
                assert!(!applies(rule, &spec), "{h:?} applies to a live node");
            }
            if covers(rule, Auth::Canned, false) {
                measured += 1;
            }
        }
        assert!(measured >= 2, "only {measured} rows state a strategy");
    }

    /// **The profile names no path**: every writable directory is a parameter, so nothing a path
    /// contains can change what the profile says.
    #[test]
    fn the_seatbelt_profile_names_no_path_only_parameters() {
        let p = SandboxPlan::seatbelt_profile(3, Some(&[0]), true, 2);
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(regex-quote (param \"SIB\"))"), "{p}");
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

    /// **A link a node left where it may write grants nothing**: its agent dir moved aside for a
    /// link, a worktree inside the agent dir, a row's path under its home, the checkout it works
    /// in, or a directory between the recorded checkout root and a deeper cwd, each swapped for a
    /// link to an outside dir, refuses the plan by name. A link *above* a checkout, which is the
    /// operator's, still resolves.
    ///
    /// Mutation: give a cwd inside the recorded checkout to `below_parent` and the plan grants
    /// the outside dir's `b`.
    #[test]
    fn a_link_in_place_of_a_granted_dir_refuses_the_plan() {
        use std::os::unix::fs::symlink;
        let rule = OsSandboxRule::Wrap {
            writes: &[WritePath::HomeProject(".claude/projects")],
        };
        let refused = |spec: &LaunchSpec, inv: &Invocation, what: &std::path::Path| {
            let err = SandboxPlan::for_launch(rule, spec, inv).unwrap_err();
            let name = what.file_name().unwrap().to_string_lossy().into_owned();
            assert!(err.contains("is a link") && err.contains(&name), "{err}");
        };
        let victim = |dir: &std::path::Path| {
            let v = dir.join("home-of-the-operator");
            std::fs::create_dir_all(&v).unwrap();
            v
        };

        let dir = marion_testsupport::scratch("osb-link-agent");
        let (spec, inv) = node_in(&dir, Auth::Canned);
        let agent = dir.join("agent");
        std::fs::rename(&agent, dir.join("wt/moved")).unwrap();
        symlink(victim(&dir), &agent).unwrap();
        refused(&spec, &inv, &agent);

        let dir = marion_testsupport::scratch("osb-link-worktree");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let wt = dir.join("agent/worktree");
        symlink(victim(&dir), &wt).unwrap();
        inv.cwd = wt.clone();
        refused(&spec, &inv, &wt);

        let dir = marion_testsupport::scratch("osb-link-row");
        let (spec, inv) = node_in(&dir, Auth::Canned);
        let plan = SandboxPlan::for_launch(rule, &spec, &inv).expect("a plan");
        let row = plan.dirs()[2].clone();
        std::fs::remove_dir(&row).unwrap();
        symlink(victim(&dir), &row).unwrap();
        refused(&spec, &inv, &row);

        let dir = marion_testsupport::scratch("osb-link-checkout");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let checkout = dir.join("checkout");
        symlink(victim(&dir), &checkout).unwrap();
        inv.cwd = checkout.clone();
        refused(&spec, &inv, &checkout);

        // Two levels inside the recorded checkout, through a link the node left at the first:
        // the cwd's parent resolves outside, and only the checkout's root may be resolved.
        let dir = marion_testsupport::scratch("osb-link-inside-checkout");
        let (mut spec, mut inv) = node_in(&dir, Auth::Canned);
        let checkout = dir.join("checkout");
        std::fs::create_dir_all(checkout.join("x/y")).unwrap();
        std::fs::create_dir_all(victim(&dir).join("b")).unwrap();
        let a = checkout.join("a");
        symlink(victim(&dir), &a).unwrap();
        spec.extra.checkout_root = Some(checkout.clone());
        inv.cwd = a.join("b");
        refused(&spec, &inv, &a);
        inv.cwd = checkout.join("x/y");
        let plan = SandboxPlan::for_launch(rule, &spec, &inv).expect("a real subdirectory");
        assert_eq!(
            plan.dirs()[1],
            std::fs::canonicalize(&checkout).unwrap().join("x/y")
        );

        let dir = marion_testsupport::scratch("osb-link-above-checkout");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        std::fs::create_dir_all(dir.join("real/wt")).unwrap();
        symlink(dir.join("real"), dir.join("alias")).unwrap();
        inv.cwd = dir.join("alias/wt");
        let plan = SandboxPlan::for_launch(rule, &spec, &inv).expect("a link above is theirs");
        assert_eq!(
            plan.dirs()[1],
            std::fs::canonicalize(dir.join("real/wt")).unwrap()
        );
    }

    /// **A `TMPDIR` inside the agent dir is writable only as part of it**: swapped for a link to
    /// an outside dir before the launch, a process under the plan writes nothing there.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_tmpdir_link_in_the_agent_dir_grants_no_write_where_it_points() {
        assert!(support().available(), "{}", support().describe());
        let dir = marion_testsupport::scratch("osb-link-tmp");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let victim = dir.join("home-of-the-operator");
        std::fs::create_dir_all(&victim).unwrap();
        let tmp = dir.join("agent/tmp");
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();
        inv.args = vec!["-c".into(), "echo tmp > \"$TMPDIR/t.txt\"".into()];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let out = inv.command(&tmp).output().expect("the process runs");
        assert!(!victim.join("t.txt").exists(), "{out:?}");
        assert_eq!(std::fs::read_dir(&victim).unwrap().count(), 0);
    }

    /// **A link swapped in while the node runs grants nothing either**: the plan names each
    /// grant by its literal path and Seatbelt resolves a path when it is used, so a process
    /// under the plan that moves its `tmp/`, its worktree and its whole agent dir aside for links
    /// to an outside dir is refused every write through them.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_link_the_running_node_swaps_in_resolves_outside_its_grants() {
        assert!(support().available(), "{}", support().describe());
        let dir = marion_testsupport::scratch("osb-link-swap");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let victim = dir.join("home-of-the-operator");
        std::fs::create_dir_all(&victim).unwrap();
        let agent = dir.join("agent");
        let wt = agent.join("worktree");
        let tmp = agent.join("tmp");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        inv.cwd = wt.clone();
        let (a, w, t, v) = (
            agent.display(),
            wt.display(),
            tmp.display(),
            victim.display(),
        );
        inv.args = vec![
            "-c".into(),
            format!(
                "mv '{t}' '{t}.away' && ln -s '{v}' '{t}' && echo x > '{t}/via-tmp'; \
                 mv '{w}' '{w}.away' && ln -s '{v}' '{w}' && echo x > '{w}/via-worktree'; \
                 mv '{a}' '{a}.away' && ln -s '{v}' '{a}' && echo x > '{a}/via-agent'; true"
            ),
        ];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let out = inv.command(&tmp).output().expect("the process runs");
        assert_eq!(
            std::fs::read_dir(&victim).unwrap().count(),
            0,
            "nothing written through a swapped link: {out:?}"
        );
        assert!(
            std::fs::symlink_metadata(&tmp)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the swap itself was allowed, so the refusal is the literal grant's: {out:?}"
        );
    }

    /// **Landlock adds no rule through a link**: a grant's components past its resolved base are
    /// opened `O_NOFOLLOW`, so one swapped for a link fails the open, and so the spawn.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_landlock_rule_is_not_opened_through_a_link() {
        let dir = marion_testsupport::scratch("osb-landlock-link");
        let base = std::fs::canonicalize(&dir).unwrap();
        std::fs::create_dir_all(base.join("agent/tmp")).unwrap();
        std::fs::create_dir_all(base.join("victim")).unwrap();
        let open = |tail: &str| {
            let rules =
                landlock::rules(&[base.join(tail)], std::slice::from_ref(&base), 0).unwrap();
            // SAFETY: open_rule only opens and closes fds over memory `rules` owns.
            let fd = unsafe { landlock::open_rule(&rules[0]) };
            if fd >= 0 {
                drop(unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
            }
            fd >= 0
        };
        assert!(open("agent/tmp"));
        std::fs::remove_dir(base.join("agent/tmp")).unwrap();
        std::os::unix::fs::symlink(base.join("victim"), base.join("agent/tmp")).unwrap();
        assert!(!open("agent/tmp"), "a linked tail component is refused");
        std::fs::rename(base.join("agent"), base.join("moved")).unwrap();
        std::os::unix::fs::symlink(base.join("victim"), base.join("agent")).unwrap();
        assert!(!open("agent"), "a linked grant root is refused");
    }

    /// **A node under its plan reads no sibling node's process**: Landlock lets a sandboxed
    /// process ptrace only into its own domain, so a sibling's `/proc/<pid>/environ` and
    /// `/proc/<pid>/mem` refuse it whatever `kernel.yama.ptrace_scope` says. Files are another
    /// matter on Linux (`platform_command`); this is what peer binding's residuals rest on (§5.4).
    /// The same read from an unsandboxed sibling succeeds, so the refusal is the sandbox's.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_under_its_plan_cannot_read_a_sibling_nodes_environment_or_memory() {
        if !support().available() {
            eprintln!("skipped: {}", support().describe());
            return;
        }
        const SECRET: &str = "PEER-SECRET-7f3a";
        let under_plan = |tag: &str, script: String| {
            let dir = marion_testsupport::scratch(tag);
            let (spec, mut inv) = node_in(&dir, Auth::Canned);
            inv.args = vec!["-c".into(), script];
            inv.env.push(("MARION_TEST_SECRET".into(), SECRET.into()));
            inv.sandbox = Some(
                SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
            );
            let tmp = dir.join("agent/tmp");
            std::fs::create_dir_all(&tmp).unwrap();
            (dir, inv.command(&tmp))
        };
        let (_victim_dir, mut victim) = under_plan("osb-ptrace-victim", "exec sleep 30".into());
        let mut victim = victim.spawn().expect("the victim runs");
        let pid = victim.id();
        let environ = format!("/proc/{pid}/environ");
        // The test is the victim's parent and unsandboxed; once it reads the secret, the
        // victim's environment is in place for the reads below.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !std::fs::read(&environ).is_ok_and(|e| String::from_utf8_lossy(&e).contains(SECRET)) {
            assert!(
                std::time::Instant::now() < deadline,
                "the victim never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let read = format!("cat {environ} 2>&1; echo '|'; head -c1 /proc/{pid}/mem 2>&1; true");
        let (_dir, mut sandboxed) = under_plan("osb-ptrace-reader", read.clone());
        let out = sandboxed.output().expect("the reader runs");
        let unsandboxed = std::process::Command::new("/bin/sh")
            .args(["-c", &read])
            .output()
            .expect("the control runs");
        let _ = victim.kill();
        let _ = victim.wait();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let reads: Vec<&str> = stdout.split('|').map(str::trim).collect();
        assert_eq!(reads.len(), 2, "{out:?}");
        for (what, read) in ["environ", "mem"].iter().zip(&reads) {
            assert!(
                !read.contains(SECRET)
                    && (read.contains("denied") || read.contains("not permitted")),
                "a sandboxed sibling read the victim's {what}: {read:?}"
            );
        }
        assert!(
            String::from_utf8_lossy(&unsandboxed.stdout).contains(SECRET),
            "an unsandboxed sibling reads the environment, so the refusal is Landlock's: \
             {unsandboxed:?}"
        );
    }

    /// **The worktree admin dir a plan writes is the one marion verified, never what the tree's
    /// `.git` names.** The node can rewrite its own gitfile; pointed at the operator's whole
    /// repository, it would otherwise be granted that repository's config on its next launch.
    /// The plan grants exactly [`Extras::git_admin_dir`], and nothing where it is `None`.
    #[test]
    fn the_admin_dir_written_is_the_verified_one_not_the_gitfiles() {
        let dir = marion_testsupport::scratch("osb-admin");
        let (mut spec, inv) = node_in(&dir, Auth::Canned);
        let planted = dir.join("operator-repo/.git");
        std::fs::create_dir_all(&planted).unwrap();
        std::fs::write(
            dir.join("wt/.git"),
            format!("gitdir: {}\n", planted.display()),
        )
        .unwrap();
        let c = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        let rule = OsSandboxRule::Wrap { writes: &[] };
        let plan = SandboxPlan::for_launch(rule, &spec, &inv).expect("a plan");
        assert!(
            !plan.dirs().contains(&c(&planted)),
            "the gitfile chose a writable dir: {:?}",
            plan.dirs()
        );
        let admin = dir.join("verified-admin");
        std::fs::create_dir_all(&admin).unwrap();
        spec.extra.git_admin_dir = Some(admin.clone());
        let plan = SandboxPlan::for_launch(rule, &spec, &inv).expect("a plan");
        assert!(plan.dirs().contains(&c(&admin)), "{:?}", plan.dirs());
        assert!(!plan.dirs().contains(&c(&planted)));
    }

    /// **No node reads another node's token**: under its plan a process reads its own agent dir
    /// and a sibling's `contracts/` and `events.jsonl` (what a parent's bridge reads of its
    /// children), and nothing else in the sibling's dir — its `config/node-token` above all.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_process_under_its_plan_reads_no_other_nodes_token() {
        assert!(support().available(), "{}", support().describe());
        let dir = marion_testsupport::scratch("osb-siblings");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let sibling = dir.join("sibling");
        std::fs::create_dir_all(sibling.join("config")).unwrap();
        std::fs::create_dir_all(sibling.join("contracts")).unwrap();
        std::fs::write(sibling.join(TOKEN_PATH), "sibling-secret").unwrap();
        std::fs::write(sibling.join("contracts/t.json"), "contract").unwrap();
        std::fs::write(sibling.join("events.jsonl"), "events").unwrap();
        std::fs::write(sibling.join("prompt.txt"), "prompt").unwrap();
        std::fs::write(dir.join("agent/config/node-token"), "own-token").unwrap();
        let s = sibling.display();
        inv.args = vec![
            "-c".into(),
            format!(
                "cat '{s}/config/node-token' '{s}/prompt.txt'; \
                 cat '{s}/contracts/t.json' '{s}/events.jsonl' ../agent/config/node-token"
            ),
        ];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let tmp = dir.join("agent/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(stdout, "contracteventsown-token", "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"),
            "{out:?}"
        );
    }

    /// **The parent tree a child may read is that one tree and nothing beside it**: with it
    /// granted, a sibling's worktree and token, a cousin's worktree, and a sibling's file reached
    /// through a link inside the granted tree are each still refused, while the tree itself reads.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_granted_parent_tree_opens_no_sibling_cousin_or_linked_path() {
        assert!(support().available(), "{}", support().describe());
        let dir = marion_testsupport::scratch("osb-parent-tree-negative");
        let (mut spec, mut inv) = node_in(&dir, Auth::Canned);
        let (parent, sibling, cousin) =
            (dir.join("parent"), dir.join("sibling"), dir.join("cousin"));
        std::fs::create_dir_all(parent.join("worktree/src")).unwrap();
        std::fs::write(parent.join("worktree/src/f.txt"), "tree").unwrap();
        for d in [&sibling, &cousin] {
            std::fs::create_dir_all(d.join("worktree")).unwrap();
            std::fs::write(d.join("worktree/f"), "SECRET-WORKTREE").unwrap();
        }
        std::fs::create_dir_all(sibling.join("config")).unwrap();
        std::fs::write(sibling.join(TOKEN_PATH), "SECRET-TOKEN").unwrap();
        std::os::unix::fs::symlink("../../sibling/worktree/f", parent.join("worktree/link"))
            .unwrap();
        spec.extra.parent_tree = Some(parent.join("worktree"));
        let denied = [
            sibling.join("worktree/f"),
            sibling.join(TOKEN_PATH),
            cousin.join("worktree/f"),
            parent.join("worktree/link"),
        ];
        let script: String = std::iter::once(parent.join("worktree/src/f.txt"))
            .chain(denied.iter().cloned())
            .map(|p| format!("cat '{}' 2>&1; echo '|'; ", p.display()))
            .collect();
        inv.args = vec!["-c".into(), script];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let tmp = dir.join("agent/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let reads: Vec<&str> = stdout.split('|').map(str::trim).collect();
        assert_eq!(reads[0], "tree", "the granted tree reads: {stdout}");
        for (path, read) in denied.iter().zip(&reads[1..]) {
            assert!(
                read.contains("Operation not permitted") && !read.contains("SECRET"),
                "{} was read: {read:?}",
                path.display()
            );
        }
    }

    /// **A node does not write what marion shows of it**: its `contracts/`, its `events.jsonl`
    /// and that file's rotated copy sit in its own writable agent dir, and a parent's bridge and
    /// the operator's view read them, so its sandbox refuses writes there — creating, replacing
    /// and removing alike — while the rest of its agent dir stays its own.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_process_under_its_plan_cannot_write_its_own_contracts_or_events() {
        assert!(support().available(), "{}", support().describe());
        let dir = marion_testsupport::scratch("osb-held");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let agent = dir.join("agent");
        std::fs::create_dir_all(agent.join("contracts")).unwrap();
        std::fs::write(agent.join("contracts/t.json"), "by marion").unwrap();
        std::fs::write(agent.join("events.jsonl"), "by marion").unwrap();
        inv.args = vec![
            "-c".into(),
            "echo forged > ../agent/contracts/t.json; echo forged > ../agent/contracts/new.json; \
             echo forged >> ../agent/events.jsonl; echo forged > ../agent/events.jsonl.1; \
             rm -f ../agent/contracts/t.json; mv ../agent/contracts ../agent/moved; \
             echo own > ../agent/scratch.txt; true"
                .into(),
        ];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let tmp = agent.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        let read = |p: &str| std::fs::read_to_string(agent.join(p)).ok();
        assert_eq!(
            read("contracts/t.json").as_deref(),
            Some("by marion"),
            "{out:?}"
        );
        assert_eq!(read("contracts/new.json"), None);
        assert_eq!(read("events.jsonl").as_deref(), Some("by marion"));
        assert_eq!(read("events.jsonl.1"), None);
        assert!(!agent.join("moved").exists());
        assert_eq!(
            read("scratch.txt").as_deref(),
            Some("own\n"),
            "the rest is its own"
        );
    }

    /// **A grant holding the agents dir is named, and on Linux refuses the launch**: Landlock can
    /// deny nothing inside a write grant, so a node working in a checkout that holds marion's
    /// state dir would read and write every sibling's token. A grant beside or below the agents
    /// dir (the node's own agent dir) is not one.
    ///
    /// Mutation: drop the `holds_no_siblings` call from `for_launch` and the Linux plan is made.
    #[test]
    fn a_grant_holding_the_agents_dir_is_refused_where_landlock_runs() {
        let dir = marion_testsupport::scratch("osb-state-in-checkout-linux");
        let checkout = std::fs::canonicalize(dir.join("."))
            .unwrap()
            .join("checkout");
        let agents = checkout.join("state");
        let grant = |p: &std::path::Path| Grant {
            base: p.parent().unwrap().to_path_buf(),
            path: p.to_path_buf(),
        };
        let err =
            holds_no_siblings(&[grant(&agents.join("me")), grant(&checkout)], &agents).unwrap_err();
        assert!(
            err.contains("MARION_STATE_DIR") && err.contains(&checkout.display().to_string()),
            "{err}"
        );
        assert!(
            holds_no_siblings(&[grant(&agents)], &agents).is_err(),
            "the dir itself"
        );
        assert!(
            holds_no_siblings(
                &[grant(&agents.join("me")), grant(&dir.join("wt"))],
                &agents
            )
            .is_ok()
        );

        let (spec, mut inv) = node_in(&agents, Auth::Canned);
        std::fs::create_dir_all(&checkout).unwrap();
        inv.cwd = checkout.clone();
        let planned = SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv);
        if cfg!(target_os = "linux") {
            let err = planned.unwrap_err();
            assert!(err.contains("MARION_STATE_DIR"), "{err}");
        } else {
            planned.expect("Seatbelt's sibling rules hold inside the grant");
        }
    }

    /// **A checkout holding marion's state dir opens no sibling's agent dir**: a node working in
    /// that checkout (a root, a `shared-cwd` child) is granted the whole checkout, and a root's
    /// child names it as its parent tree, yet a sibling's token is neither read nor written,
    /// while the checkout itself and the node's own agent dir stay the node's.
    ///
    /// Mutation: emit the writable directories' read allows after the sibling deny again, or grant
    /// a parent tree outside the agents dir, and the token reads.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_checkout_holding_the_state_dir_opens_no_siblings_token() {
        assert!(support().available(), "{}", support().describe());
        let dir = marion_testsupport::scratch("osb-state-in-checkout");
        let checkout = dir.join("checkout");
        let agents = checkout.join("state");
        let sibling = agents.join("sibling");
        for parent_tree in [None, Some(checkout.clone())] {
            let (mut spec, mut inv) = node_in(&agents, Auth::Canned);
            std::fs::create_dir_all(sibling.join("config")).unwrap();
            std::fs::write(sibling.join(TOKEN_PATH), "SECRET-TOKEN").unwrap();
            spec.extra.parent_tree = parent_tree.clone();
            inv.cwd = checkout.clone();
            let s = sibling.display();
            inv.args = vec![
                "-c".into(),
                format!(
                    "cat '{s}/config/node-token' 2>&1; echo forged > '{s}/config/node-token'; \
                     echo '|'; echo work > f.txt && cat f.txt; echo '|'; \
                     echo own > state/agent/own.txt && cat state/agent/own.txt; true"
                ),
            ];
            inv.sandbox = Some(
                SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
            );
            let tmp = agents.join("agent/tmp");
            std::fs::create_dir_all(&tmp).unwrap();
            let out = inv.command(&tmp).output().expect("the process runs");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let parts: Vec<&str> = stdout.split('|').map(str::trim).collect();
            assert!(
                parts[0].contains("Operation not permitted") && !stdout.contains("SECRET"),
                "{parent_tree:?}: {out:?}"
            );
            assert_eq!(
                std::fs::read_to_string(sibling.join(TOKEN_PATH)).unwrap(),
                "SECRET-TOKEN",
                "{parent_tree:?}: not written"
            );
            assert_eq!(
                parts[1], "work",
                "{parent_tree:?}: the checkout is the node's"
            );
            assert_eq!(parts[2], "own", "{parent_tree:?}: its own agent dir too");
        }
    }

    /// **A child of a node reads the parent's worktree it was spawned over, and nothing else of
    /// the parent's.** That tree is its bridge's `MARION_REPO`, which the bridge must resolve to
    /// reach its supervisor at all; it sits in the parent's agent dir, beside the parent's token.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_process_under_its_plan_reads_its_parent_tree_and_not_the_parents_token() {
        assert!(support().available(), "{}", support().describe());
        let dir = marion_testsupport::scratch("osb-parent-tree");
        let (mut spec, mut inv) = node_in(&dir, Auth::Canned);
        let parent = dir.join("parent");
        std::fs::create_dir_all(parent.join("config")).unwrap();
        std::fs::create_dir_all(parent.join("worktree/src")).unwrap();
        std::fs::write(parent.join(TOKEN_PATH), "parent-secret").unwrap();
        std::fs::write(parent.join("worktree/src/f.txt"), "tree").unwrap();
        spec.extra.parent_tree = Some(parent.join("worktree"));
        let p = parent.display();
        inv.args = vec![
            "-c".into(),
            format!(
                "cd '{p}/worktree' && pwd -P >/dev/null && cat src/f.txt; \
                 cat '{p}/config/node-token'; true"
            ),
        ];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let tmp = dir.join("agent/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "tree", "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"),
            "{out:?}"
        );
    }

    /// **A hard link from the node's own dirs does not read a sibling's token either.** Seatbelt
    /// matches the path a read names, so a link the node made in its own agent dir or workspace
    /// to a sibling's `config/node-token` would be a second name for the file under a path the
    /// read rule allows; making that link must be refused.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_hard_link_from_its_own_dirs_reads_no_other_nodes_token() {
        assert!(support().available(), "{}", support().describe());
        let dir = marion_testsupport::scratch("osb-hardlink");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let sibling = dir.join("sibling");
        std::fs::create_dir_all(sibling.join("config")).unwrap();
        std::fs::write(sibling.join(TOKEN_PATH), "sibling-secret").unwrap();
        let s = sibling.display();
        inv.args = vec![
            "-c".into(),
            format!(
                "ln '{s}/config/node-token' ../agent/linked; cat ../agent/linked; \
                 ln '{s}/config/node-token' linked; cat linked; \
                 ln '{s}/config' ../agent/dirlink; cat ../agent/dirlink/node-token; true"
            ),
        ];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let tmp = dir.join("agent/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("sibling-secret"),
            "a hard link read the sibling's token: {out:?}"
        );
        assert!(!dir.join("agent/linked").exists(), "{out:?}");
        assert!(!dir.join("wt/linked").exists(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"),
            "refused by the sandbox, not by chance: {out:?}"
        );
    }

    /// **Linux: a node writes nothing marion shows of another node.** Landlock restricts writes
    /// only and grants whole directories, so a node's own `contracts/` and `events.jsonl` stay its
    /// to write there (docs/os-sandbox.md); a sibling's, and its token, sit outside every grant and
    /// are refused — creating, appending, replacing and removing alike — while the node's own agent
    /// dir stays writable.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_under_its_plan_cannot_write_a_siblings_contracts_events_or_token() {
        if !support().available() {
            eprintln!("skipped: {}", support().describe());
            return;
        }
        let dir = marion_testsupport::scratch("osb-held-linux");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let sibling = dir.join("sibling");
        std::fs::create_dir_all(sibling.join("config")).unwrap();
        std::fs::create_dir_all(sibling.join("contracts")).unwrap();
        std::fs::write(sibling.join("contracts/t.json"), "by marion").unwrap();
        std::fs::write(sibling.join("events.jsonl"), "by marion").unwrap();
        std::fs::write(sibling.join(TOKEN_PATH), "sibling-secret").unwrap();
        let s = sibling.display();
        inv.args = vec![
            "-c".into(),
            format!(
                "echo forged > '{s}/contracts/t.json'; echo forged > '{s}/contracts/new.json'; \
                 echo forged >> '{s}/events.jsonl'; echo forged > '{s}/events.jsonl.1'; \
                 echo forged > '{s}/{TOKEN_PATH}'; rm -f '{s}/contracts/t.json'; \
                 mv '{s}/contracts' '{s}/moved'; echo own > ../agent/scratch.txt; true"
            ),
        ];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let tmp = dir.join("agent/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        let read = |p: &str| std::fs::read_to_string(sibling.join(p)).ok();
        assert_eq!(
            read("contracts/t.json").as_deref(),
            Some("by marion"),
            "{out:?}"
        );
        assert_eq!(read("contracts/new.json"), None, "{out:?}");
        assert_eq!(read("events.jsonl").as_deref(), Some("by marion"));
        assert_eq!(read("events.jsonl.1"), None);
        assert_eq!(read(TOKEN_PATH).as_deref(), Some("sibling-secret"));
        assert!(!sibling.join("moved").exists(), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("agent/scratch.txt"))
                .ok()
                .as_deref(),
            Some("own\n"),
            "its own agent dir is its own: {out:?}"
        );
    }

    /// **Linux: a hard link into its own dirs writes no other node's token.** A link the node
    /// made in its writable agent dir or workspace to a sibling's `config/node-token` would be a
    /// second name for that file under a granted path, and a write through it would rewrite the
    /// token. Landlock refuses the link itself (it would give the file a write right it has not
    /// got where it is), so the token is never rewritten.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hard_link_into_its_own_dirs_writes_no_other_nodes_token() {
        if !support().available() {
            eprintln!("skipped: {}", support().describe());
            return;
        }
        let dir = marion_testsupport::scratch("osb-hardlink-linux");
        let (spec, mut inv) = node_in(&dir, Auth::Canned);
        let sibling = dir.join("sibling");
        std::fs::create_dir_all(sibling.join("config")).unwrap();
        std::fs::write(sibling.join(TOKEN_PATH), "sibling-secret").unwrap();
        let s = sibling.display();
        inv.args = vec![
            "-c".into(),
            format!(
                "ln '{s}/{TOKEN_PATH}' ../agent/linked; echo forged >> ../agent/linked; \
                 ln '{s}/{TOKEN_PATH}' linked; echo forged >> linked; true"
            ),
        ];
        inv.sandbox = Some(
            SandboxPlan::for_launch(OsSandboxRule::Wrap { writes: &[] }, &spec, &inv).unwrap(),
        );
        let tmp = dir.join("agent/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = inv.command(&tmp).output().expect("the process runs");
        assert_eq!(
            std::fs::read_to_string(sibling.join(TOKEN_PATH)).unwrap(),
            "sibling-secret",
            "a hard link rewrote the sibling's token: {out:?}"
        );
        // The `>>` after a refused `ln` makes a file of the node's own there: one name, no secret.
        for linked in [dir.join("agent/linked"), dir.join("wt/linked")] {
            let Ok(meta) = std::fs::symlink_metadata(&linked) else {
                continue;
            };
            assert_eq!(
                std::os::unix::fs::MetadataExt::nlink(&meta),
                1,
                "{} is a second name for the token: {out:?}",
                linked.display()
            );
            assert!(
                !std::fs::read_to_string(&linked)
                    .unwrap_or_default()
                    .contains("sibling-secret"),
                "{out:?}"
            );
        }
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
