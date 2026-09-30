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
    self, AbortVerb, Arg, Boot, DialogAnswer, HarnessSpec, McpRoute, NodeShape, Push, Spelling,
    Surfaces, TurnDelivery, UpdatePolicy, Val,
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
    for d in dialogs.dialogs {
        let normal = d.needle.split_whitespace().collect::<Vec<_>>().join(" ");
        if d.needle.is_empty() || d.needle != normal {
            fault(
                "boot_dialogs",
                format!("needle {:?} is not written as a screen is read", d.needle),
            );
        }
        if unstated(d.note) || unstated(d.action) {
            fault(
                "boot_dialogs",
                format!("{:?} states no note or no action", d.needle),
            );
        }
        if matches!(d.answer, DialogAnswer::Keys(k) if k.is_empty()) {
            fault(
                "boot_dialogs",
                format!("{:?} answers with nothing", d.needle),
            );
        }
    }
    if dialogs
        .dialogs
        .iter()
        .any(|d| matches!(d.answer, DialogAnswer::Keys(_)))
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
    for v in row.verified {
        if v.is_empty() || !v.split('.').all(|p| p.parse::<u64>().is_ok()) {
            fault("version.verified", format!("{v:?} is not a dotted version"));
        }
    }
    if (1..row.verified.len()).any(|i| row.verified[..i].contains(&row.verified[i])) {
        fault("version.verified", "a version repeats".into());
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
                    spelling: Spelling::PerAgent,
                    ..qwen
                },
                "spelling",
                "spelling: a spelling per agent needs a row whose program arrives per launch",
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
