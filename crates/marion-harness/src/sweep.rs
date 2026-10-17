//! **One set of invariants for every row, built in or read from a file.**
//!
//! A row is a transcription of measurements, and most of what makes one wrong is visible in the
//! row alone: a note that says nothing, a verb outside marion's vocabulary, a declaration that
//! rides argv with the node token in it, a turn delivery the row's surfaces cannot carry. The
//! built-in rows are held to these by the sweep test below; a row read from a file is held to the
//! same ones at load ([`crate::row_file`]), where a fault is refused with the key it names. What
//! needs a rendered launch or a captured fixture to judge stays in the adapter's own sweeps.

use std::fmt;

use marion_core::agent_type;

use crate::spec::{
    self, AbortVerb, Arg, Boot, DialogAnswer, HarnessSpec, McpRoute, NodeShape, Push, Remembers,
    Spelling, Surfaces, TurnDelivery, UpdatePolicy, Val,
};
use crate::surfaces::TypedKind;

/// One broken invariant: the row key it is about and what is wrong, in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowFault {
    pub key: &'static str,
    pub message: String,
}

impl fmt::Display for RowFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

/// Every fault in `row`, in key order of the checks below; empty for a row that holds.
pub fn validate(row: &HarnessSpec) -> Vec<RowFault> {
    let mut faults = Vec::new();
    let mut fault = |key: &'static str, message: String| faults.push(RowFault { key, message });
    let unstated = |note: &str| note.trim().is_empty();

    if unstated(row.note) || !row.note.chars().any(|c| c == 'S' || c == 's') {
        fault(
            "note",
            "the row names no measurement it was taken from".into(),
        );
    }
    for (verb, native) in row.tool_names {
        if !agent_type::TOOL_VOCABULARY.contains(verb) {
            fault("tools", format!("`{verb}` is not one of marion's verbs"));
        }
        if native.trim().is_empty() {
            fault("tools", format!("`{verb}` maps to no tool of the harness"));
        }
    }
    match row.spelling {
        Spelling::Fixed(s) if !s.spell("report").contains(spec::MCP_ALIAS) => fault(
            "spelling",
            "the spelling does not name marion's server".into(),
        ),
        // The agent is what spells, so the row's program must be the launch's, not its own.
        Spelling::PerAgent if row.program.is_some() => fault(
            "spelling",
            "a spelling per agent needs a row whose program arrives per launch".into(),
        ),
        _ => {}
    }
    for (key, mode, route, carrier) in [
        ("mcp.canned", "canned", row.mcp.canned, row.token.canned),
        ("mcp.live", "live", row.mcp.live, row.token.live),
    ] {
        if route == McpRoute::None {
            fault(key, "a declaration always has a route".into());
        }
        // `ps` shows argv to every user on the machine.
        if matches!(route, McpRoute::Argv(_)) && !carrier.withholds() {
            fault(
                key,
                format!("the {mode} declaration rides argv and carries the node token"),
            );
        }
    }
    if let Some(d) = row.live_declaration
        && d.route() != row.mcp.live
    {
        fault(
            "declaration",
            "the live declaration is not on the row's live route".into(),
        );
    }
    let boot = row.boot;
    if unstated(boot.note()) {
        fault(
            "boot",
            "a boot cost without the measurement behind it".into(),
        );
    }
    if boot.budget() < spec::BOOT_FLOOR {
        fault("boot", "a boot budget under the floor".into());
    }
    if let Boot::Measured { cpu, .. } = boot
        && cpu.is_zero()
    {
        fault("boot", "a measured boot of no CPU at all".into());
    }
    let update_note = match row.updates {
        UpdatePolicy::Env { note, .. }
        | UpdatePolicy::Pair { note, .. }
        | UpdatePolicy::Document { note, .. }
        | UpdatePolicy::Never { note }
        | UpdatePolicy::None { note } => note,
    };
    if unstated(update_note) {
        fault(
            "updates",
            "an update policy without the measurement behind it".into(),
        );
    }
    for shape in [NodeShape::Headless, NodeShape::Interactive] {
        delivery_faults(row, shape, &mut fault);
        abort_faults(row, shape, &mut fault);
    }
    let dialogs = row.boot_dialogs;
    if unstated(dialogs.note) {
        fault(
            "boot_dialogs",
            "boot dialogs without the measurement behind them".into(),
        );
    }
    let needles = dialogs.dialogs.iter().map(|d| d.needle);
    let needles = needles.chain(
        dialogs
            .asks
            .iter()
            .flat_map(|a| [a.header, a.footer, a.option]),
    );
    for needle in needles {
        let normal = needle.split_whitespace().collect::<Vec<_>>().join(" ");
        if needle.is_empty() || needle != normal {
            fault(
                "boot_dialogs",
                format!("needle {needle:?} is not written as a screen is read"),
            );
        }
    }
    for a in dialogs.asks {
        if unstated(a.note) || unstated(a.action) {
            fault(
                "boot_dialogs",
                format!("ask {:?} states no note or no action", a.header),
            );
        }
    }
    for d in dialogs.dialogs {
        if unstated(d.note) || unstated(d.action) {
            fault(
                "boot_dialogs",
                format!("{:?} states no note or no action", d.needle),
            );
        }
        if d.answer.keys().is_some_and(<[u8]>::is_empty) {
            fault(
                "boot_dialogs",
                format!("{:?} answers with nothing", d.needle),
            );
        }
    }
    if dialogs.dialogs.iter().any(|d| d.answer.keys().is_some())
        && dialogs
            .dialogs
            .last()
            .is_none_or(|d| d.answer != DialogAnswer::Hold)
    {
        fault(
            "boot_dialogs",
            "an answered dialog needs a held, marker-free needle after it".into(),
        );
    }
    // First-run setup is kept in the harness config, so a row answering it says that config is the
    // one an overlay relocates: kept nowhere past the session, it would not be setup.
    if dialogs.remembers == Remembers::Nothing
        && let Some(d) = dialogs
            .dialogs
            .iter()
            .find(|d| matches!(d.answer, DialogAnswer::Setup(_)))
    {
        fault(
            "boot_dialogs",
            format!(
                "{:?} is first-run setup, kept in the harness config, but the row says it \
                 remembers nothing",
                d.needle
            ),
        );
    }
    if unstated(row.approval.note()) {
        fault(
            "approval",
            "an approval without the measurement behind it".into(),
        );
    }
    if unstated(row.read_only.note()) {
        fault(
            "read_only",
            "a read-only switch without the measurement behind it".into(),
        );
    }
    let login = row.login_env;
    if !login.any_provider && login.login.is_empty() {
        fault(
            "login_env",
            "states no login variable and reaches no provider".into(),
        );
    }
    for g in login.login {
        let body = g.pattern.trim_end_matches('*');
        if body.is_empty() || body.contains('*') || body != body.to_uppercase() {
            fault(
                "login_env",
                format!("{:?} is not a variable name or a prefix", g.pattern),
            );
        }
    }
    match row.program {
        None if !row.verified.is_empty() => fault(
            "version.verified",
            "a row whose version is not its own states none".into(),
        ),
        Some(_) if row.verified.is_empty() => {
            fault("version.verified", "states no verified version".into())
        }
        _ => {}
    }
    for p in row.repo_config {
        if !plain_relative(p) {
            fault(
                "repo_config",
                format!("{p:?} is not a plain path under the repository's checkout"),
            );
        }
    }
    for v in row.verified {
        if v.is_empty() || !v.split('.').all(|p| p.parse::<u64>().is_ok()) {
            fault("version.verified", format!("{v:?} is not a dotted version"));
        }
    }
    if (1..row.verified.len()).any(|i| row.verified[..i].contains(&row.verified[i])) {
        fault("version.verified", "a version repeats".into());
    }
    for r in row.requires {
        match r.need {
            spec::Need::PathAbsent(p) if !std::path::Path::new(p).is_absolute() => fault(
                "requires",
                format!("{p:?} is not an absolute path, so it would be read from marion's cwd"),
            ),
            spec::Need::EnvUnset(k) if k.trim().is_empty() => {
                fault("requires", "an env-unset need names no variable".into())
            }
            _ => {}
        }
    }
    for file in row.files {
        if !plain_relative(file.path) {
            fault(
                "files",
                format!(
                    "{:?} is not a plain path under the node's directory",
                    file.path
                ),
            );
        }
        if file.render(None).is_none() {
            fault(
                "files",
                format!("{:?}'s base is not a JSON object", file.path),
            );
        }
        if !file.borrowed_login.is_empty() && file.modes != spec::Modes::All {
            fault(
                "files",
                format!(
                    "{:?} guards a borrowed login, so it must be written under every auth mode",
                    file.path
                ),
            );
        }
        if file.servers.is_some() && file.servers_key.trim().is_empty() {
            fault(
                "files",
                format!("{:?} declares servers under no key", file.path),
            );
        }
    }
    let mut seen = Vec::new();
    for r in row.wires {
        if unstated(r.note) {
            fault("wires", format!("{:?} has no note", r.wire));
        }
        if seen.contains(&r.wire) {
            fault("wires", format!("{:?} twice", r.wire));
        }
        seen.push(r.wire);
        for (k, _) in r.env {
            if row
                .env
                .iter()
                .any(|e| e.key == *k && matches!(e.val, Val::Under(_)))
            {
                fault("wires", format!("recipe variable {k} relocates state"));
            }
        }
    }
    faults
}

/// A turn delivery the row's surfaces can carry, with its measurement.
fn delivery_faults(
    row: &HarnessSpec,
    shape: NodeShape,
    fault: &mut impl FnMut(&'static str, String),
) {
    let d = spec::delivery_for(row, shape);
    let headless = shape == NodeShape::Headless;
    if d.note().trim().is_empty() {
        fault(
            "delivery",
            format!("{shape:?}: a turn delivery without the measurement behind it"),
        );
    }
    let carried = match d {
        TurnDelivery::TypedTurn { .. } => {
            headless
                && matches!(
                    row.surfaces,
                    Surfaces::Headless(_) | Surfaces::JsonlRpc(_) | Surfaces::AppServer(_)
                )
        }
        TurnDelivery::Continuation { .. } => {
            headless
                && row.surfaces == Surfaces::LaunchOnly
                && row.resume.is_some()
                && row.argv.contains(&Arg::Resume)
        }
        TurnDelivery::McpChannel { .. } => !headless && matches!(row.push, Push::Channel(_)),
        TurnDelivery::TerminalPaste { submit, .. } => {
            !headless
                && (row.pane.is_some() || row.live_declaration.is_some())
                && !submit.is_empty()
        }
        TurnDelivery::None { .. } => true,
    };
    if !carried {
        fault(
            "delivery",
            format!("{shape:?}: the row's surfaces cannot carry this turn delivery"),
        );
    }
}

/// An abort verb the row has a way to send, with its measurement.
fn abort_faults(row: &HarnessSpec, shape: NodeShape, fault: &mut impl FnMut(&'static str, String)) {
    let verb = spec::abort_for(row, shape);
    let headless = shape == NodeShape::Headless;
    if verb.note().trim().is_empty() {
        fault(
            "abort",
            format!("{shape:?}: an abort verb without the measurement behind it"),
        );
    }
    if verb.grace_ms() > spec::MAX_CANCEL_GRACE_MS {
        fault("abort", format!("{shape:?}: a grace past the cancel bound"));
    }
    let sendable = match verb {
        AbortVerb::Channel { grace_ms, .. } => {
            headless
                && grace_ms > 0
                && matches!(
                    row.surfaces,
                    Surfaces::JsonlRpc(_)
                        | Surfaces::Headless(TypedKind::Acp | TypedKind::AppServer)
                )
        }
        AbortVerb::Keys { keys, grace_ms, .. } => {
            !headless
                && (row.pane.is_some() || row.live_declaration.is_some())
                && grace_ms > 0
                && !keys.is_empty()
                && keys.iter().all(|k| !k.is_empty())
        }
        AbortVerb::None { .. } => true,
    };
    if !sendable {
        fault(
            "abort",
            format!("{shape:?}: the row has no way to send this abort"),
        );
    }
}

/// A non-empty relative path of plain components only: no root, no `.` or `..`.
fn plain_relative(p: &str) -> bool {
    let path = std::path::Path::new(p);
    !p.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::harness_spec;
    use marion_core::harness::Harness;

    /// **Every built-in row holds every invariant a row read from a file is held to.**
    #[test]
    fn every_built_in_row_passes_the_row_validator() {
        for h in Harness::ALL {
            let faults = validate(harness_spec(h));
            assert!(faults.is_empty(), "{h}: {faults:#?}");
        }
    }

    /// Each fault names its key and says what is wrong, one golden per kind of mistake a row
    /// file can make.
    #[test]
    fn a_broken_row_is_refused_with_the_key_it_breaks() {
        let qwen = *harness_spec(Harness::Qwen);
        let cases: Vec<(HarnessSpec, &str, &str)> = vec![
            (
                HarnessSpec { note: "", ..qwen },
                "note",
                "note: the row names no measurement it was taken from",
            ),
            (
                HarnessSpec {
                    tool_names: &[("shell", "run_shell_command")],
                    ..qwen
                },
                "tools",
                "tools: `shell` is not one of marion's verbs",
            ),
            (
                HarnessSpec {
                    token: spec::TokenCarriers {
                        live: spec::TokenCarrier::Declaration,
                        ..qwen.token
                    },
                    ..qwen
                },
                "mcp.live",
                "mcp.live: the live declaration rides argv and carries the node token",
            ),
            (
                HarnessSpec {
                    updates: UpdatePolicy::None { note: " " },
                    ..qwen
                },
                "updates",
                "updates: an update policy without the measurement behind it",
            ),
            (
                HarnessSpec {
                    verified: &[],
                    ..qwen
                },
                "version.verified",
                "version.verified: states no verified version",
            ),
            (
                HarnessSpec {
                    files: Box::leak(Box::new([spec::ConfigFile {
                        path: "../settings.json",
                        ..qwen.files[0]
                    }])),
                    ..qwen
                },
                "files",
                "files: \"../settings.json\" is not a plain path under the node's directory",
            ),
            (
                HarnessSpec {
                    files: Box::leak(Box::new([spec::ConfigFile {
                        base: "[]",
                        ..qwen.files[0]
                    }])),
                    ..qwen
                },
                "files",
                "files: \"home/settings.json\"'s base is not a JSON object",
            ),
            (
                HarnessSpec {
                    requires: Box::leak(Box::new([spec::Requirement {
                        modes: spec::Modes::All,
                        need: spec::Need::PathAbsent("etc/hermes"),
                        why: "synthetic",
                    }])),
                    ..qwen
                },
                "requires",
                "requires: \"etc/hermes\" is not an absolute path, so it would be read from \
                 marion's cwd",
            ),
            (
                HarnessSpec {
                    files: Box::leak(Box::new([spec::ConfigFile {
                        servers_key: " ",
                        ..qwen.files[0]
                    }])),
                    ..qwen
                },
                "files",
                "files: \"home/settings.json\" declares servers under no key",
            ),
            (
                HarnessSpec {
                    files: Box::leak(Box::new([spec::ConfigFile {
                        borrowed_login: &[("auth.adopt_external_logins", false)],
                        ..qwen.files[0]
                    }])),
                    ..qwen
                },
                "files",
                "files: \"home/settings.json\" guards a borrowed login, so it must be written \
                 under every auth mode",
            ),
            (
                HarnessSpec {
                    spelling: Spelling::PerAgent,
                    ..qwen
                },
                "spelling",
                "spelling: a spelling per agent needs a row whose program arrives per launch",
            ),
            (
                HarnessSpec {
                    repo_config: &["../.mcp.json"],
                    ..qwen
                },
                "repo_config",
                "repo_config: \"../.mcp.json\" is not a plain path under the repository's checkout",
            ),
            (
                HarnessSpec {
                    boot_dialogs: spec::BootDialogs {
                        dialogs: &[spec::BootDialog {
                            needle: "› 1. Open restricted 2. Quit",
                            action: "open {repo} restricted",
                            answer: spec::DialogAnswer::Decline(b""),
                            note: "test: a declining answer of no keys",
                        }],
                        ..qwen.boot_dialogs
                    },
                    ..qwen
                },
                "boot_dialogs",
                "boot_dialogs: \"› 1. Open restricted 2. Quit\" answers with nothing",
            ),
            (
                HarnessSpec {
                    boot_dialogs: spec::BootDialogs {
                        dialogs: &[spec::BootDialog {
                            needle: "› 1. Open restricted 2. Quit",
                            action: "open {repo} restricted",
                            answer: spec::DialogAnswer::Decline(b"\r"),
                            note: "test: a declining answer with no held needle after it",
                        }],
                        ..qwen.boot_dialogs
                    },
                    ..qwen
                },
                "boot_dialogs",
                "boot_dialogs: an answered dialog needs a held, marker-free needle after it",
            ),
            (
                HarnessSpec {
                    boot_dialogs: spec::BootDialogs {
                        dialogs: Box::leak(Box::new([
                            spec::BootDialog {
                                needle: "Pick a theme",
                                action: "run qwen once and pick a theme",
                                answer: DialogAnswer::Setup(b"\r"),
                                note: "measured",
                            },
                            spec::BootDialog {
                                needle: "theme",
                                action: "run qwen once and pick a theme",
                                answer: DialogAnswer::Hold,
                                note: "measured",
                            },
                        ])),
                        asks: &[],
                        remembers: Remembers::Nothing,
                        note: "measured",
                    },
                    ..qwen
                },
                "boot_dialogs",
                "boot_dialogs: \"Pick a theme\" is first-run setup, kept in the harness config, \
                 but the row says it remembers nothing",
            ),
        ];
        for (row, key, golden) in cases {
            let faults = validate(&row);
            assert!(
                faults
                    .iter()
                    .any(|f| f.key == key && f.to_string() == golden),
                "{key}: {faults:#?}"
            );
        }
    }
}
