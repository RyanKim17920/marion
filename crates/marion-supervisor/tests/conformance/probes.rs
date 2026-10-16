//! **The battery: every probe written once, reading the row.**
//!
//! No probe names a harness. Each reads what the row states — its surfaces (through the launch
//! path), its update policy, its approval grant, its stream grammar, its turn delivery, its resume
//! grammar, its pane shape — decides from that what the harness should do, drives it, and reports
//! PASS, FAIL, or UNSUPPORTED with the row's own reason. A new row gets the whole battery.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_core::contract::{FailureCause, TokenUsage};
use marion_harness::grammar;
use marion_harness::spec::{
    Approval, Arg, Field, HarnessSpec, MidTurn, NodeShape, TurnDelivery, UpdatePolicy,
};
use marion_harness::stream::{CallOutcome, ChildExit, json_frames};
use marion_supervisor::duplex::LaunchPath;
use serde_json::{Value, json};

use crate::driver::{self, BOOT, Gate, Node, Proc};
use crate::provider::{Call, Provider, Sent, Turn};
use crate::report::{Log, Outcome, Scrub};
use crate::target::{self, Knobs, Launch, Target, World};

/// The probes, in matrix order.
pub const PROBES: &[&str] = &[
    "P-version",
    "P-launch",
    "P-tools",
    "P-activity",
    "P-approval",
    "P-midturn",
    "P-interrupt",
    "P-resume",
    "P-lifecycle",
    "P-errors",
    "P-tui",
    "P-untrusted-mcp",
];

/// A healthy scripted turn on any harness is seconds; this bounds a wedged one.
const TURN: Duration = Duration::from_secs(90);
/// How long a faulted run may take before the probe records it as still retrying.
const FAULT_BOUND: Duration = Duration::from_secs(45);
/// The readiness probe's bridge delay: well past every measured "wait a moment" (codex
/// app-server's ~1 s), well inside `BOOT`.
const SLOW_BRIDGE_SECS: u64 = 4;

/// One target's run: where its fixtures go, where its scratch worlds live.
pub struct Ctx<'a> {
    pub t: &'a Target,
    pub out: PathBuf,
    pub scratch: PathBuf,
    /// Facts one probe hands a later one, so the battery does not run the same turn twice.
    pub report_answered: Option<bool>,
    /// A canned stand-in run in place of the row's program, for the battery's own tests.
    pub stand_in: Option<StandIn>,
    /// The committed transcripts of the row's newest verified release, where the matrix has them:
    /// the wire an unlisted release is held to ([`ask_shape_drift`]). Cleared for a listed one.
    pub verified: Option<PathBuf>,
}

/// A program launched in place of the row's, with everything the row compiled plus `env` and
/// `MARION_STAND_IN_PROVIDER` (the probe's provider URL, spelled the same for every row).
pub struct StandIn {
    pub program: PathBuf,
    pub env: Vec<(String, String)>,
}

/// One probe's world, provider and transcript.
struct Session {
    world: World,
    log: Arc<Log>,
    provider: Provider,
}

impl Ctx<'_> {
    fn session(&self, probe: &str, part: &str) -> Session {
        let name = if part.is_empty() {
            probe.to_string()
        } else {
            format!("{probe}-{part}")
        };
        let world = World::new(&self.scratch, &name.to_ascii_lowercase());
        let log = Arc::new(Log::create(
            &self
                .out
                .join(format!("{}.jsonl", name.to_ascii_lowercase())),
            Scrub::new(&self.scratch),
        ));
        let provider = Provider::start(&world.root, Arc::clone(&log), self.t.report_name());
        Session {
            world,
            log,
            provider,
        }
    }

    /// A turn that calls marion's `report` `reports` times, then says `final_text`.
    fn turn(&self, marker: &str, reports: usize, final_text: &str) -> Turn {
        let call = Call {
            spelled: self.t.report_name(),
            verb: "report".into(),
            args: json!({"narrative": format!("conformance {marker}")}),
        };
        Turn {
            marker: marker.into(),
            calls: vec![call; reports],
            final_text: final_text.into(),
        }
    }

    fn compile(&self, s: &Session, prompt: &str, knobs: &Knobs) -> Result<Launch, String> {
        let mut launch = target::compile(self.t, &s.world, &s.provider.base_url(), prompt, knobs)?;
        if let Some(si) = &self.stand_in {
            launch.inv.program = si.program.display().to_string();
            launch.inv.env.extend(si.env.iter().cloned());
            launch
                .inv
                .env
                .push(("MARION_STAND_IN_PROVIDER".into(), s.provider.base_url()));
        }
        Ok(launch)
    }
}

/// The prompt a probe sends: the marker the provider keys on, and nothing that spells a tool.
fn prompt(marker: &str) -> String {
    format!("{marker}: follow the conformance script.")
}

fn exit_word(s: Option<std::process::ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match s {
        Some(s) => match (s.code(), s.signal()) {
            (Some(c), _) => format!("exit {c}"),
            (_, Some(sig)) => format!("signal {sig}"),
            _ => "exit ?".into(),
        },
        None => "still running".into(),
    }
}

fn child_exit(p: &Proc) -> ChildExit {
    use std::os::unix::process::ExitStatusExt;
    let s = p.with(|io| io.exit);
    ChildExit {
        code: s.and_then(|s| s.code()),
        signal: s.and_then(|s| s.signal()),
        timed_out: false,
    }
}

/// Run every probe against one target.
pub fn run_all(c: &mut Ctx<'_>) -> (String, Vec<Outcome>) {
    let (version, v) = p_version(c);
    // A listed release is the bar itself: a person listed it having read its wire, so its own run
    // re-records that wire rather than being held to the release before it.
    if c.t.spec.verified.contains(&version.as_str()) {
        c.verified = None;
    }
    eprintln!(
        "conformance: {} {} {}: {}",
        c.t.selector,
        v.probe,
        v.status.word(),
        v.observed
    );
    let mut outcomes = vec![v];
    let probes: [fn(&mut Ctx<'_>) -> Outcome; 11] = [
        p_launch,
        p_tools,
        p_activity,
        p_approval,
        p_midturn,
        p_interrupt,
        p_resume,
        p_lifecycle,
        p_errors,
        p_tui,
        p_untrusted_mcp,
    ];
    // `MARION_CONFORMANCE_PROBES=P-tui,P-errors` runs only those; the rest are not run, and the
    // matrix and the row's directory keep their last results for the same version.
    let only: Option<Vec<String>> = std::env::var("MARION_CONFORMANCE_PROBES")
        .ok()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());
    for (name, probe) in PROBES[1..].iter().zip(probes) {
        if only.as_ref().is_some_and(|o| !o.iter().any(|p| p == name)) {
            outcomes.push(Outcome::not_run(name));
            continue;
        }
        let o = probe(c);
        eprintln!(
            "conformance: {} {} {}: {}",
            c.t.selector,
            o.probe,
            o.status.word(),
            o.observed
        );
        outcomes.push(o);
    }
    (version, outcomes)
}

/// A frame that asks marion for permission: an ACP `session/request_permission`, or a
/// `can_use_tool` control request.
fn is_ask(f: &Value) -> bool {
    f["method"] == "session/request_permission" || f["request"]["subtype"] == "can_use_tool"
}

/// Every key path in `v` (`request.tool_name`, `params.options[].kind`), values ignored.
fn key_paths(v: &Value, prefix: &str, out: &mut std::collections::BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                key_paths(val, &path, out);
                out.insert(path);
            }
        }
        Value::Array(a) => a
            .iter()
            .for_each(|x| key_paths(x, &format!("{prefix}[]"), out)),
        _ => {}
    }
}

/// The asks' key paths, over every ask.
fn ask_shape(asks: &[Value]) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    asks.iter().for_each(|a| key_paths(a, "", &mut out));
    out
}

/// **How `asked`'s shape differs from the asks in the verified release's committed P-approval
/// transcripts under `dir`** — `+path` for a key only the live asks carry, `-path` for one only the
/// recorded asks did — or `None` where they agree or either side asked nothing to compare.
fn ask_shape_drift(dir: &Path, asked: &[Value]) -> Option<String> {
    let recorded: Vec<Value> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("p-approval"))
        .flat_map(|e| {
            std::fs::read_to_string(e.path())
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .map(|l| l["msg"].clone())
                .filter(is_ask)
                .collect::<Vec<_>>()
        })
        .collect();
    if recorded.is_empty() || asked.is_empty() {
        return None;
    }
    let (was, now) = (ask_shape(&recorded), ask_shape(asked));
    let moved: Vec<String> = now
        .difference(&was)
        .map(|p| format!("+{p}"))
        .chain(was.difference(&now).map(|p| format!("-{p}")))
        .collect();
    (!moved.is_empty()).then(|| moved.join(", "))
}

// --- P-version ----------------------------------------------------------------------------------

/// The installed version, read the way the gate reads it (`--version` under the row's
/// no-self-update switch) — or, for an ACP agent, off its own `initialize` answer — and whether the
/// row's no-self-update switch reaches the compiled launch.
fn p_version(c: &mut Ctx<'_>) -> (String, Outcome) {
    const P: &str = "P-version";
    let s = c.session(P, "");
    let launch = match c.compile(&s, &prompt("CONFVERSION"), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            let o =
                Outcome::unsupported(P, format!("marion will not compile a canned launch: {e}"));
            return ("unknown".into(), o);
        }
    };
    let version = match c.t.path {
        LaunchPath::Acp => acp_version(&launch, &s.log),
        _ => flag_version(&launch.inv.program, c.t.spec),
    };
    let (policy_ok, policy) = update_policy_reaches(c.t.spec.updates, &launch);
    // An ACP agent's reading is `<name> <version>`.
    let listed = version
        .as_deref()
        .and_then(|v| v.split_whitespace().last())
        .is_some_and(|v| {
            // An ACP agent's releases are its program's row's.
            marion_testsupport::gated_row(&launch.inv.program)
                .map_or(c.t.spec.verified, |s| s.verified)
                .contains(&v)
        });
    let admitted = if listed {
        "listed in the row's verified releases"
    } else {
        "not listed in the row's verified releases"
    };
    s.log
        .note(format!("version {version:?}; {policy}; {admitted}"));
    let observed = format!(
        "{} ({admitted}); {policy}",
        version.as_deref().unwrap_or("no version read")
    );
    let o = Outcome::judged(
        P,
        version.is_some() && policy_ok,
        "a version is read, and the row's no-self-update switch is in the compiled launch",
        observed,
    );
    (version.unwrap_or_else(|| "unknown".into()), o)
}

/// Through `marion_harness::probe`, the constructor every marion probe uses, so the switch here is
/// the row's in whatever shape it has; a row that probe refuses has no version read.
fn flag_version(program: &str, spec: &HarnessSpec) -> Option<String> {
    let mut probe = marion_harness::probe::version_probe(spec, program).ok()?;
    let out = probe.command.output().ok()?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    marion_supervisor::doctor::version_token(&text)
}

/// An ACP agent's version is what its `initialize` says — the doctor's rule (§3.3).
fn acp_version(launch: &Launch, log: &Arc<Log>) -> Option<String> {
    let p = Proc::spawn(&launch.inv, true, Arc::clone(log), None).ok()?;
    p.send(&marion_harness::acp::initialize_request(1));
    let mut info = None;
    p.wait(BOOT, |io| {
        info = io
            .frames
            .iter()
            .find(|f| f["id"] == 1)
            .map(|f| f["result"]["agentInfo"].clone());
        info.is_some() || io.exit.is_some()
    });
    let info = info?;
    let name = info["name"].as_str().unwrap_or("agent");
    info["version"].as_str().map(|v| format!("{name} {v}"))
}

fn update_policy_reaches(policy: UpdatePolicy, launch: &Launch) -> (bool, String) {
    match policy {
        UpdatePolicy::Env { key, value, .. } => {
            let ok = launch.inv.env.iter().any(|(k, v)| k == key && v == value);
            (
                ok,
                format!("no-self-update env {key}={value} {}", carried(ok)),
            )
        }
        UpdatePolicy::Pair { key, value, .. } => {
            let pair = format!("{key}={value}");
            let ok = launch.inv.args.iter().any(|a| a == &pair);
            (ok, format!("no-self-update pair {pair} {}", carried(ok)))
        }
        UpdatePolicy::Document { keys, .. } => {
            let docs: Vec<Value> = launch
                .files
                .iter()
                .filter_map(|f| std::fs::read_to_string(f).ok())
                .filter_map(|s| serde_json::from_str(&s).ok())
                .collect();
            let ok = keys.iter().all(|(path, want)| {
                let ptr = format!("/{}", path.replace('.', "/"));
                docs.iter()
                    .any(|d| d.pointer(&ptr) == Some(&Value::Bool(*want)))
            });
            (ok, format!("no-self-update document keys {}", carried(ok)))
        }
        UpdatePolicy::Never { note } => (true, format!("binary never updates itself ({note})")),
        UpdatePolicy::None { note } => (true, format!("row states no switch ({note})")),
    }
}

fn carried(ok: bool) -> &'static str {
    if ok {
        "carried"
    } else {
        "MISSING from the compiled launch"
    }
}

// --- P-launch -----------------------------------------------------------------------------------

fn p_launch(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-launch";
    let s = c.session(P, "");
    let marker = "CONFLAUNCH";
    s.provider
        .hold
        .add_turn(c.turn(marker, 0, "conformance launch ok"));
    let launch = match c.compile(&s, &prompt(marker), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
    };
    let node = match Node::start(
        c.t.path,
        &launch,
        &prompt(marker),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => {
            return Outcome::judged(P, false, expected_launch(c), format!("did not start: {e}"));
        }
    };
    let ended = node.wait_ends(1, TURN);
    let seen = s.provider.hold.seen();
    let asked = seen
        .iter()
        .any(|r| r.turn.as_ref().is_some_and(|(m, _)| m == marker));
    let wires: std::collections::BTreeSet<String> =
        seen.iter().filter_map(|r| r.wire.clone()).collect();
    let stderr = node.proc.stderr();
    let auth = marion_harness::auth_failure_line(&stderr);
    let exit = node.proc.with(|io| io.exit);
    let clean_exit =
        !matches!(c.t.path, LaunchPath::LaunchOnly) || exit.is_some_and(|e| e.success());
    let observed = format!(
        "turn {}; provider asked for the marker {asked} on {wires:?}; {}; declaration route \
         verified at compile; auth failure line: {}",
        if ended { "ended" } else { "did not end" },
        exit_word(exit),
        auth.as_deref().unwrap_or("none"),
    );
    Outcome::judged(
        P,
        ended && asked && clean_exit && auth.is_none(),
        expected_launch(c),
        observed,
    )
}

fn expected_launch(c: &Ctx<'_>) -> String {
    format!(
        "the headless launch (surfaces {:?}) takes one canned turn and ends it{}, with marion's \
         declaration on the route {:?}",
        c.t.spec.surfaces,
        if matches!(c.t.path, LaunchPath::LaunchOnly) {
            " at exit 0"
        } else {
            ""
        },
        c.t.spec.mcp.canned,
    )
}

// --- P-tools ------------------------------------------------------------------------------------

/// Whether the model's first request lists marion's tools, and what the harness does when marion's
/// MCP server is slow to start — waits, or sends without it (codex app-server's hazard, S36).
fn p_tools(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-tools";
    let marker = "CONFTOOLS";
    let normal = first_request(c, "normal", marker, None, Gate::Marion);
    let (normal_tools, _) = match normal {
        Ok(v) => v,
        Err(Refused::Compile(e)) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
        Err(Refused::Run(e)) => return Outcome::judged(P, false, expected_tools(c), e),
    };
    let s_dir = c.scratch.join("slowbridge");
    std::fs::create_dir_all(&s_dir).expect("slow bridge dir");
    let slow = s_dir.join("bridge.sh");
    std::fs::write(
        &slow,
        format!(
            "#!/bin/sh\nsleep {SLOW_BRIDGE_SECS}\nexec '{}' \"$@\"\n",
            target::bridge().display()
        ),
    )
    .expect("write slow bridge");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let delayed = first_request(c, "slow-bridge", marker, Some(slow), Gate::None);
    let (slow_tools, after) = match delayed {
        Ok(v) => v,
        Err(Refused::Compile(e) | Refused::Run(e)) => (false, format!("no request: {e}")),
    };
    // The duplex path waits for the bridge's ready file; the app-server path for the server's own
    // startup notification (S36 P3).
    let marion_gates = matches!(c.t.path, LaunchPath::Duplex | LaunchPath::AppServer);
    let waits = slow_tools;
    let observed = format!(
        "first request lists `{}`: {normal_tools}; with the bridge {SLOW_BRIDGE_SECS} s slow and \
         no gate of marion's, the first request came {after} and {} marion's tools{}",
        c.t.report_name(),
        if slow_tools { "listed" } else { "did NOT list" },
        if marion_gates && !waits {
            " (marion gates the first prompt on marion's server being ready)"
        } else {
            ""
        }
    );
    Outcome::judged(
        P,
        normal_tools && (waits || marion_gates),
        expected_tools(c),
        observed,
    )
}

fn expected_tools(c: &Ctx<'_>) -> String {
    format!(
        "`{}` is in the first model request; a slow MCP server is waited for, or marion gates the \
         first prompt itself",
        c.t.report_name()
    )
}

enum Refused {
    Compile(String),
    Run(String),
}

/// Launch, and read the first request that belongs to the probe's turn: whether it listed marion's
/// tools, and when it came relative to the spawn.
///
/// **The turn's request, not a side request carrying the prompt**: opencode's ACP agent titles the
/// session by sending the prompt with no tools at all, and that request says nothing about which
/// tools the model was offered. The first marker request offering any tool is the turn's; only if
/// none offers one is the first marker request read.
fn first_request(
    c: &Ctx<'_>,
    part: &str,
    marker: &str,
    bridge: Option<PathBuf>,
    gate: Gate,
) -> Result<(bool, String), Refused> {
    let s = c.session("P-tools", part);
    s.provider
        .hold
        .add_turn(c.turn(marker, 0, "conformance tools ok"));
    let knobs = Knobs {
        bridge,
        ..Knobs::default()
    };
    let launch = c
        .compile(&s, &prompt(marker), &knobs)
        .map_err(Refused::Compile)?;
    let spawned_at = s.log.secs();
    let node = Node::start(c.t.path, &launch, &prompt(marker), Arc::clone(&s.log), gate)
        .map_err(Refused::Run)?;
    let got = s.provider.hold.wait(TURN, |seen, _| {
        seen.iter()
            .any(|r| r.turn.as_ref().is_some_and(|(m, _)| m == marker))
    });
    node.wait_ends(1, TURN);
    if !got {
        return Err(Refused::Run(
            "the harness never asked the provider for the turn".into(),
        ));
    }
    let asked: Vec<_> = s
        .provider
        .hold
        .seen()
        .into_iter()
        .filter(|r| r.turn.as_ref().is_some_and(|(m, _)| m == marker))
        .collect();
    let first = asked
        .iter()
        .find(|r| r.offers_tools)
        .or(asked.first())
        .expect("waited for above");
    let after = format!("{:.1} s after the spawn", first.t - spawned_at);
    s.log.note(format!(
        "{part}: first request {after}, marion tools {}",
        first.marion_tools
    ));
    Ok((first.marion_tools, after))
}

// --- P-activity ---------------------------------------------------------------------------------

/// One scripted turn — a `report`, then text, with counted usage — read back through the row's own
/// reader: marion's calls and their verdicts, the narrative, the usage, the session id, the
/// activity peek. The usage must be **what the provider sent**, every request's counters added:
/// budgets read nothing else, so a reader that misses a request, double-counts a cache or keeps
/// the wrong report of two is spend marion cannot see.
fn p_activity(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-activity";
    let s = c.session(P, "");
    let marker = "CONFACTIVITY";
    s.provider
        .hold
        .add_turn(c.turn(marker, 1, "conformance activity done"));
    s.provider.hold.count_usage();
    let launch = match c.compile(&s, &prompt(marker), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
    };
    let node = match Node::start(
        c.t.path,
        &launch,
        &prompt(marker),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => {
            return Outcome::judged(
                P,
                false,
                "a scripted turn runs",
                format!("did not start: {e}"),
            );
        }
    };
    let ended = node.wait_ends(1, TURN);
    let r = read_turn(c, &node);
    c.report_answered = Some(r.report == Some(CallOutcome::Answered));
    let g = c.t.spec.stream;
    let wants_usage = g.is_none_or(|g| g.usage.is_some());
    let wants_session = g.is_none_or(|g| g.session.is_some());
    let wants_activity = g.is_some_and(|g| g.activity.is_some());
    let sent = s.provider.hold.usage_sent();
    // A row that says its figure leaves out the requests outside its turn is held to the rest.
    let uncounted = g.and_then(|g| g.usage.as_ref()).and_then(|u| u.uncounted);
    let expected_usage = sent_total(&sent, uncounted.is_some());
    let usage_ok = r.usage.is_some_and(|u| same_counters(&u, &expected_usage));
    let shown_usage = r.usage.map(|u| format!("{u:?}"));
    let shown_sent: Vec<String> = sent
        .iter()
        .map(|s| {
            let (i, o, c) = s.counts;
            format!(
                "({i}, {o}, {c}){}",
                if s.offers_tools { "" } else { " no tools" }
            )
        })
        .collect();
    let observed = format!(
        "turn {}; report {:?}; narrative {:?}; usage {} (provider sent [{}]{}); session {} ; \
         activity {}",
        if ended { "ended" } else { "did not end" },
        r.report,
        r.narrative,
        shown_usage.as_deref().unwrap_or(if wants_usage {
            "NONE"
        } else {
            "none (row: no usage rule)"
        }),
        shown_sent.join(", "),
        uncounted.map_or(String::new(), |u| format!("; row leaves out: {u}")),
        r.session.as_deref().unwrap_or(if wants_session {
            "NONE"
        } else {
            "none (row: no session rule)"
        }),
        r.activity.as_deref().unwrap_or(if wants_activity {
            "NONE"
        } else {
            "none (row: no activity rule)"
        }),
    );
    let pass = ended
        && r.report == Some(CallOutcome::Answered)
        && r.narrative.is_some()
        && (!wants_usage || usage_ok)
        && (!wants_session || r.session.is_some())
        && (!wants_activity || r.activity.is_some());
    Outcome::judged(
        P,
        pass,
        "the row's reader finds `report` answered, its narrative, and every unit the row says the \
         stream carries (session id, activity, and usage equal to every request's counters the \
         provider sent, added; only the tool-offering ones where the row says it leaves the rest \
         out)",
        observed,
    )
}

/// Every counted request's spend, added, in marion's shape: `input` uncached, beside `cache_read`.
/// `turn_only`: just the requests that offered tools, for a row whose figure leaves the rest out.
fn sent_total(sent: &[Sent], turn_only: bool) -> TokenUsage {
    sent.iter()
        .filter(|s| s.offers_tools || !turn_only)
        .fold(TokenUsage::default(), |t, s| {
            let (input, output, cached) = s.counts;
            TokenUsage {
                input: t.input + input,
                output: t.output + output,
                cache_read: t.cache_read + cached,
                ..t
            }
        })
}

/// The four disjoint counters agree; `reasoning` is a part of `output` the provider may not split.
fn same_counters(a: &TokenUsage, b: &TokenUsage) -> bool {
    (a.input, a.output, a.cache_read, a.cache_write)
        == (b.input, b.output, b.cache_read, b.cache_write)
}

struct Read {
    report: Option<CallOutcome>,
    narrative: Option<String>,
    usage: Option<TokenUsage>,
    session: Option<String>,
    activity: Option<String>,
}

fn read_turn(c: &Ctx<'_>, node: &Node) -> Read {
    let stdout = node.proc.stdout();
    let frames = json_frames(&stdout);
    let calls = c.t.adapter.marion_calls(&stdout);
    let report = calls
        .iter()
        .find(|m| m.verb == "report")
        .map(|m| m.outcome.clone());
    let outcome = c.t.adapter.parse_stream(&stdout, child_exit(&node.proc));
    let usage =
        c.t.adapter
            .usage(&frames)
            .filter(|u| u.input + u.output + u.cache_read > 0);
    let session = match node.path {
        LaunchPath::Acp => node.session.clone(),
        _ => {
            c.t.spec
                .stream
                .and_then(|g| frames.iter().find_map(|f| grammar::session_id(g, f)))
        }
    };
    let activity =
        c.t.spec
            .stream
            .and_then(|g| g.activity.as_ref())
            .and_then(|a| {
                let r = grammar::recent_activity(a, &frames, 8);
                (!r.calls.is_empty()).then(|| {
                    r.calls
                        .iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>()
                        .join(",")
                })
            });
    Read {
        report,
        narrative: outcome.narrative,
        usage,
        session,
        activity,
    }
}

// --- P-approval ---------------------------------------------------------------------------------

/// The row's approval grant: with it, marion's `report` is answered (P-activity's turn); without it
/// — the grant stripped from the compiled launch, per the strategy the row names — the harness asks
/// or refuses. Where the surface asks over the protocol, marion's own reply is the one sent.
fn p_approval(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-approval";
    let granted = c.report_answered;
    let approval = c.t.spec.approval;
    let expected = format!(
        "granted ({}), `report` is answered; ungranted, the harness asks or refuses",
        approval.kind()
    );
    // A row that declares approve-all has no grant to strip: there is nothing to measure, as for
    // any strategy a row declares none of, and doctor warns instead.
    if let Approval::ApproveAll { note } = approval {
        return Outcome::unsupported(
            P,
            format!("the row declares approve-all: every tool runs without asking ({note})"),
        );
    }
    let Some(granted) = granted else {
        return Outcome::unsupported(P, "P-activity did not run, so the granted case is unknown");
    };
    let strip: Box<dyn Fn(&mut Launch)> = match approval {
        Approval::AllowedToolsArg { flag, .. } | Approval::CliFlag { flag, .. } => {
            Box::new(move |l: &mut Launch| {
                strip_flag(
                    &mut l.inv.args,
                    flag,
                    matches!(approval, Approval::AllowedToolsArg { .. }),
                )
            })
        }
        Approval::EnvVar { key, .. } => {
            Box::new(move |l: &mut Launch| l.inv.env.retain(|(k, _)| k != key))
        }
        // Where the harness's default already runs the tool, the ungranted case is an operator's
        // config contesting it (the row's `contest`) with marion's key gone: that is the case the
        // key exists for, and stripping it alone would pass whether or not the key works.
        Approval::DeclarationKey { key, contest, .. } => Box::new(move |l: &mut Launch| {
            let carriers = documents_with(l, key);
            strip_key(l, key);
            if let Some(c) = contest {
                contest_documents(&carriers, c);
            }
        }),
        // No mode set is marion's own default for every built-in: the agent's own permission ask
        // then reaches marion, and marion answers it with the agent's allow option.
        Approval::SessionMode { .. } => Box::new(|_: &mut Launch| {}),
        Approval::OperatorAllowlist { rule, file, .. } => {
            return Outcome::unsupported(
                P,
                format!("the grant is the operator's own `{rule}` in ~/{file}; marion writes none"),
            );
        }
        Approval::ApproveAll { .. } => unreachable!("answered above"),
    };
    let s = c.session(P, "ungranted");
    let marker = "CONFAPPROVAL";
    s.provider
        .hold
        .add_turn(c.turn(marker, 1, "conformance approval done"));
    let mut launch = match c.compile(&s, &prompt(marker), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
    };
    strip(&mut launch);
    let contested = match approval {
        Approval::DeclarationKey {
            contest: Some(c), ..
        } => format!("; operator config contesting it: {c}"),
        _ => String::new(),
    };
    s.log.note(format!(
        "grant ({}) stripped{contested}: argv {:?}",
        approval.kind(),
        launch.inv.args
    ));
    let node = match Node::start(
        c.t.path,
        &launch,
        &prompt(marker),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => {
            return Outcome::judged(
                P,
                false,
                expected,
                format!("ungranted launch did not start: {e}"),
            );
        }
    };
    node.wait_ends(1, TURN);
    let r = read_turn(c, &node);
    let asked: Vec<Value> = node
        .proc
        .with(|io| io.frames.iter().filter(|f| is_ask(f)).cloned().collect());
    // The ask is the frame marion answers, so its shape is wire marion depends on: a key the
    // verified release did not send, or one it sent and this one does not, is a change a person
    // reads before this release is admitted, however marion's reader happens to cope with it.
    if let Some(drift) = c
        .verified
        .as_deref()
        .and_then(|dir| ask_shape_drift(dir, &asked))
    {
        return Outcome::judged(
            P,
            false,
            expected,
            format!("the permission ask's shape moved against the verified release: {drift}"),
        );
    }
    let session_mode = matches!(approval, Approval::SessionMode { .. });
    if session_mode && asked.is_empty() && r.report == Some(CallOutcome::Answered) {
        // The strategy's claim is that marion answers the agent's ask with the agent's own allow
        // option; an agent that asked nothing never put that answer to the test.
        return Outcome::unsupported(
            P,
            format!(
                "granted: `report` answered {granted}; ungranted: `report` answered with no \
                 permission ask reaching marion, so marion's answer to an ask was not exercised"
            ),
        );
    }
    let ungranted_ok = if session_mode {
        r.report == Some(CallOutcome::Answered) && !asked.is_empty()
    } else {
        r.report != Some(CallOutcome::Answered) || !asked.is_empty()
    };
    let observed = format!(
        "granted: `report` answered {granted}; ungranted: `report` {:?}, {} permission ask(s) \
         reached marion{}",
        r.report,
        asked.len(),
        if !session_mode && r.report == Some(CallOutcome::Answered) && asked.is_empty() {
            " — the grant is NOT load-bearing: the harness ran marion's tool without it"
        } else {
            ""
        }
    );
    Outcome::judged(P, granted && ungranted_ok, expected, observed)
}

fn strip_flag(args: &mut Vec<String>, flag: &str, takes_value: bool) {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args.drain(..) {
        if skip {
            skip = false;
            continue;
        }
        if a == flag {
            skip = takes_value;
            continue;
        }
        if a.starts_with(&format!("{flag}=")) {
            continue;
        }
        out.push(a);
    }
    *args = out;
}

/// Remove a declaration key from every document the launch wrote and from argv's override pairs.
/// The JSON documents among the launch's files that carry `key` anywhere.
fn documents_with(l: &Launch, key: &str) -> Vec<std::path::PathBuf> {
    l.files
        .iter()
        .filter(|f| {
            std::fs::read_to_string(f).is_ok_and(|t| match serde_json::from_str::<Value>(&t) {
                Ok(v) => has_key(&v, key),
                // TOML, read the way `strip_key` strips it: a line that sets the key.
                Err(_) => t.lines().any(|line| line.trim_start().starts_with(key)),
            })
        })
        .cloned()
        .collect()
}

fn has_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(o) => o.contains_key(key) || o.values().any(|x| has_key(x, key)),
        Value::Array(a) => a.iter().any(|x| has_key(x, key)),
        _ => false,
    }
}

/// Merge `contest` into the top level of each document: a JSON object into a JSON document, and
/// `key = value` lines into a TOML one, each replacing the top-level line that set its key.
fn contest_documents(files: &[std::path::PathBuf], contest: &str) {
    for f in files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        let merged = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(mut doc)) => {
                let Ok(Value::Object(add)) = serde_json::from_str::<Value>(contest) else {
                    panic!("a JSON document's contest is a JSON object: {contest}");
                };
                doc.extend(add);
                Value::Object(doc).to_string()
            }
            Ok(_) => continue,
            Err(_) => contest_toml(&text, contest),
        };
        let _ = std::fs::write(f, merged);
    }
}

/// `contest`'s `key = value` lines ahead of `text`'s own, whose top-level lines setting the same
/// keys are dropped — a TOML key may be set once, and only above the first table.
fn contest_toml(text: &str, contest: &str) -> String {
    let key_of = |line: &str| line.split('=').next().map(|k| k.trim().to_string());
    let keys: Vec<String> = contest.lines().filter_map(key_of).collect();
    let mut top = true;
    let kept = text.lines().filter(|line| {
        top &= !line.trim_start().starts_with('[');
        !(top && key_of(line).is_some_and(|k| keys.contains(&k)))
    });
    contest
        .lines()
        .map(str::to_string)
        .chain(kept.map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_key(l: &mut Launch, key: &str) {
    for f in &l.files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        let stripped = match serde_json::from_str::<Value>(&text) {
            Ok(mut v) => {
                remove_key(&mut v, key);
                v.to_string()
            }
            Err(_) => text
                .lines()
                .filter(|line| !line.trim_start().starts_with(key))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let _ = std::fs::write(f, stripped);
    }
    let mut out = Vec::new();
    let mut prev: Option<String> = None;
    for a in l.inv.args.drain(..) {
        if a.contains(key) && a.contains('=') {
            // Drop the pair and the flag that introduced it.
            if prev.as_deref().is_some_and(|p| p.starts_with('-')) {
                out.pop();
            }
            prev = None;
            continue;
        }
        prev = Some(a.clone());
        out.push(a);
    }
    l.inv.args = out;
}

fn remove_key(v: &mut Value, key: &str) {
    match v {
        Value::Object(o) => {
            o.remove(key);
            o.values_mut().for_each(|x| remove_key(x, key));
        }
        Value::Array(a) => a.iter_mut().for_each(|x| remove_key(x, key)),
        _ => {}
    }
}

// --- P-midturn ----------------------------------------------------------------------------------

/// A message written while a turn is held at the provider, judged against the row's headless
/// delivery: folded into the running turn, queued as the next, or lost.
fn p_midturn(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-midturn";
    let delivery = c.t.adapter.turn_delivery(NodeShape::Headless);
    let mid_turn = match delivery {
        TurnDelivery::TypedTurn { mid_turn, .. } => mid_turn,
        TurnDelivery::Continuation { note } => {
            return Outcome::unsupported(
                P,
                format!(
                    "headless delivery is Continuation (a relaunch per turn, no mid-turn channel): {note}"
                ),
            );
        }
        other => return Outcome::unsupported(P, format!("headless delivery {other:?}")),
    };
    let s = c.session(P, "");
    let (a, b) = ("CONFMIDA", "CONFMIDB");
    // Two tool rounds, held after the first: the write lands while the turn still has a tool
    // result to send, which is where S31 saw a fold happen (written during the turn's *last*
    // request, the same harness queues it instead).
    s.provider
        .hold
        .add_turn(c.turn(a, 2, "conformance first done"));
    s.provider
        .hold
        .add_turn(c.turn(b, 0, "conformance second done"));
    s.provider.hold.park(a, 1);
    let launch = match c.compile(&s, &prompt(a), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
    };
    let mut node = match Node::start(
        c.t.path,
        &launch,
        &prompt(a),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => return Outcome::judged(P, false, "", format!("did not start: {e}")),
    };
    if !s.provider.hold.wait_parked(TURN) {
        return Outcome::judged(P, false, "", "the turn never reached its second request");
    }
    if let Err(e) = node.prompt(&prompt(b)) {
        return Outcome::judged(P, false, "", e);
    }
    // No surface acknowledges a mid-turn write; give the harness a moment to read it before the
    // release, as S31 did.
    node.proc.wait(Duration::from_secs(1), |_| false);
    s.provider.hold.release();
    node.wait_ends(1, TURN);
    // A queued message is its own turn and ends promptly; a fold ends with the first. Only a bound
    // can tell "no second end" apart from "not yet".
    node.wait_ends(2, Duration::from_secs(15));
    let ends = node.proc.with(|io| node.ends(io));
    let seen = s.provider.hold.seen();
    let carried: Vec<usize> = seen
        .iter()
        .filter(|r| r.body.to_string().contains(b))
        .map(|r| r.idx)
        .collect();
    let answers = node.prompt_answers();
    let fate = if carried.is_empty() {
        "dropped (no request ever carried it)".to_string()
    } else if ends >= 2 {
        format!("queued as its own turn ({ends} turn ends; requests {carried:?})")
    } else {
        format!("folded into the running turn ({ends} turn end; requests {carried:?})")
    };
    // An ACP prompt's answer is its turn's end; on the other typed surfaces it is not.
    let lost: Vec<String> = answers
        .iter()
        .filter(|_| c.t.path == LaunchPath::Acp)
        .filter(|(_, f)| f.get("error").is_some() || f["result"]["stopReason"] != "end_turn")
        .map(|(id, f)| format!("prompt {id}: {}", f.get("error").unwrap_or(&f["result"])))
        .collect();
    let first_answered = match c.t.path {
        LaunchPath::Acp => answers.len() >= 2 && lost.is_empty(),
        _ => ends >= 1,
    };
    let observed = format!(
        "{fate}{}{}{}",
        if c.t.path == LaunchPath::Acp {
            format!("; {} prompt answer(s)", answers.len())
        } else {
            String::new()
        },
        if lost.is_empty() {
            String::new()
        } else {
            format!(", not clean: {lost:?}")
        },
        match mid_turn {
            MidTurn::Queue if first_answered && !carried.is_empty() =>
                " — the harness lost nothing: the row's Queue is conservative here",
            _ => "",
        }
    );
    match mid_turn {
        MidTurn::Fold => Outcome::judged(
            P,
            !carried.is_empty() && first_answered,
            "row Fold: the write is folded into the running turn or queued as the next; nothing is \
             dropped and every prompt is answered",
            observed,
        ),
        MidTurn::Queue => Outcome::judged(
            P,
            true,
            "row Queue: marion holds a mid-turn message until the turn ends (measured here: what the \
             harness does if written anyway)",
            observed,
        ),
    }
}

// --- P-interrupt --------------------------------------------------------------------------------

/// A turn held at its first request, cancelled the surface's way (ACP `session/cancel`, a
/// stream-json interrupt, SIGINT to a launch-only group), then marion's own kill sweep.
fn p_interrupt(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-interrupt";
    let s = c.session(P, "");
    let marker = "CONFINTERRUPT";
    s.provider
        .hold
        .add_turn(c.turn(marker, 1, "conformance interrupt done"));
    s.provider.hold.park(marker, 0);
    let launch = match c.compile(&s, &prompt(marker), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
    };
    let mut node = match Node::start(
        c.t.path,
        &launch,
        &prompt(marker),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => return Outcome::judged(P, false, "", format!("did not start: {e}")),
    };
    if !s.provider.hold.wait_parked(TURN) {
        return Outcome::judged(P, false, "", "the turn never reached the provider");
    }
    node.proc.snapshot();
    let at = Instant::now();
    node.cancel();
    let stopped = node.proc.wait(Duration::from_secs(10), |io| {
        node.ends(io) >= 1 || io.exit.is_some()
    });
    let soft = if stopped {
        let exit = node.proc.with(|io| io.exit);
        format!(
            "the cancel ended the turn in {:.1} s ({})",
            at.elapsed().as_secs_f64(),
            exit_word(exit)
        )
    } else {
        "the cancel did not end the turn within 10 s".to_string()
    };
    node.proc.snapshot();
    let children = node
        .proc
        .stragglers()
        .into_iter()
        .filter(|(p, _)| *p != node.proc.pid)
        .count();
    let swept = marion_supervisor::kill::kill_process_tree_and_wait(node.proc.pid);
    let left = settle(&node.proc);
    let observed = format!(
        "{soft}; {children} descendant(s) alive after it; marion's kill sweep {} and left {:?}",
        if swept {
            "confirmed the node dead"
        } else {
            "could not confirm death"
        },
        left
    );
    Outcome::judged(
        P,
        left.is_empty(),
        "the surface's cancel ends the turn; marion's kill sweep leaves no process behind",
        observed,
    )
}

/// The stragglers once exits have had a moment to land: `ps` is the only witness of another
/// process's death, so this re-reads it at a short interval under a bound.
fn settle(p: &Proc) -> Vec<(i32, String)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = p.stragglers();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// --- P-resume -----------------------------------------------------------------------------------

/// Kill a node after its first turn, relaunch it through the row's resume spelling (ACP:
/// `session/load`; a thread channel: `thread/resume`), and check the second life carries the first
/// life's history.
fn p_resume(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-resume";
    // ACP and a thread channel resume over the protocol (`session/load`, `thread/resume`).
    if c.t.path != LaunchPath::Acp && c.t.spec.resume.is_none() && c.t.spec.surfaces.rpc().is_none()
    {
        return Outcome::unsupported(P, "the row states no resume spelling (`resume: None`)");
    }
    let s = c.session(P, "");
    let (m1, m2) = ("CONFRESUMEONE", "CONFRESUMETWO");
    s.provider
        .hold
        .add_turn(c.turn(m1, 1, "conformance first life done"));
    s.provider
        .hold
        .add_turn(c.turn(m2, 0, "conformance second life done"));
    let launch = match c.compile(&s, &prompt(m1), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
    };
    let first = match Node::start(
        c.t.path,
        &launch,
        &prompt(m1),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => return Outcome::judged(P, false, "", format!("first life did not start: {e}")),
    };
    first.wait_ends(1, TURN);
    if c.t.path == LaunchPath::Acp {
        let loads = first.proc.with(|io| {
            io.frames
                .iter()
                .find(|f| f["id"] == driver::ACP_INIT_ID)
                .map(|f| f["result"]["agentCapabilities"]["loadSession"] == true)
        });
        if loads != Some(true) {
            return Outcome::unsupported(P, "the agent does not advertise `loadSession`");
        }
    }
    let session = read_turn(c, &first).session;
    first.proc.signal_group(driver::SIGKILL);
    first.proc.wait_exit(Duration::from_secs(10));
    drop(first);
    let Some(id) = session else {
        return Outcome::judged(P, false, "", "the first life named no session to resume");
    };
    let knobs = Knobs {
        resume: Some(id.clone()),
        ..Knobs::default()
    };
    let launch = match c.compile(&s, &prompt(m2), &knobs) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::judged(P, false, "", format!("the resume does not compile: {e}"));
        }
    };
    let second = match Node::start(
        c.t.path,
        &launch,
        &prompt(m2),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => return Outcome::judged(P, false, "", format!("second life did not start: {e}")),
    };
    let ended = second.wait_ends(1, TURN);
    let carried = s.provider.hold.seen().iter().any(|r| {
        let text = r.body.to_string();
        text.contains(m2) && text.contains(m1)
    });
    let stdout = second.proc.stdout();
    let refusal = c.t.adapter.resume_refusal(&stdout, Some(&id));
    let after = read_turn(c, &second).session;
    let observed = format!(
        "second life {}; its request carries the first life's prompt: {carried}; session {} -> \
         {}; resume refusal: {}",
        if ended { "ended" } else { "did not end" },
        id,
        after.as_deref().unwrap_or("none"),
        refusal.as_deref().unwrap_or("none"),
    );
    Outcome::judged(
        P,
        ended && carried && refusal.is_none(),
        "the row's resume spelling continues the killed session with its history",
        observed,
    )
}

// --- P-lifecycle --------------------------------------------------------------------------------

/// How a node ends: idle stdin EOF (typed surfaces) or a finished turn (launch-only), and SIGTERM
/// mid-turn — each with its exit code, and nothing it started left alive.
fn p_lifecycle(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-lifecycle";
    let s = c.session(P, "idle");
    let marker = "CONFLIFEIDLE";
    s.provider
        .hold
        .add_turn(c.turn(marker, 0, "conformance idle done"));
    let launch = match c.compile(&s, &prompt(marker), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
    };
    let node = match Node::start(
        c.t.path,
        &launch,
        &prompt(marker),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => return Outcome::judged(P, false, "", format!("did not start: {e}")),
    };
    node.wait_ends(1, TURN);
    node.proc.snapshot();
    if !matches!(c.t.path, LaunchPath::LaunchOnly) {
        node.proc.close_stdin();
    }
    let idle_exit = node.proc.wait_exit(Duration::from_secs(20));
    let idle_left = settle(&node.proc);
    drop(node);

    let s = c.session(P, "sigterm");
    let marker = "CONFLIFETERM";
    s.provider
        .hold
        .add_turn(c.turn(marker, 1, "conformance term done"));
    s.provider.hold.park(marker, 1);
    let launch = match c.compile(&s, &prompt(marker), &Knobs::default()) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(
                P,
                format!("marion will not compile a canned launch: {e}"),
            );
        }
    };
    let node = match Node::start(
        c.t.path,
        &launch,
        &prompt(marker),
        Arc::clone(&s.log),
        Gate::Marion,
    ) {
        Ok(n) => n,
        Err(e) => return Outcome::judged(P, false, "", format!("did not start: {e}")),
    };
    let parked = s.provider.hold.wait_parked(TURN);
    node.proc.snapshot();
    // SIGTERM to the harness alone — its own children are its to end.
    unsafe_kill(node.proc.pid, driver::SIGTERM);
    let term_exit = node.proc.wait_exit(Duration::from_secs(20));
    // Judged on what is left once the node has ended as marion ends one — its session swept before
    // the leader's reap (`Proc`'s waiter, `kill::end_node`) — and on nothing the probe adds.
    let term_left = settle(&node.proc);
    let idle_ok = idle_exit.is_some_and(|e| e.success());
    let observed = format!(
        "{}: {}, left {:?}; SIGTERM mid-turn{}: {}, left {:?} (each after marion's end of the node)",
        if matches!(c.t.path, LaunchPath::LaunchOnly) {
            "turn end"
        } else {
            "idle stdin EOF"
        },
        exit_word(idle_exit),
        idle_left,
        if parked {
            ""
        } else {
            " (turn never reached its tool round)"
        },
        exit_word(term_exit),
        term_left,
    );
    Outcome::judged(
        P,
        idle_ok && idle_left.is_empty() && term_exit.is_some() && term_left.is_empty(),
        "exit 0 at stdin EOF or turn end; SIGTERM ends the harness; nothing it started outlives the \
         node",
        observed,
    )
}

fn unsafe_kill(pid: i32, sig: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: a plain libc call on a pid this probe spawned and has not reaped.
    let _ = unsafe { kill(pid, sig) };
}

// --- P-errors -----------------------------------------------------------------------------------

/// The provider answers every turn request with 401, 429 or 500. What the harness said by the
/// bound — its stderr and the provider errors its row's grammar reads off its stream — must
/// classify, through the one classifier marion's contracts use (`HarnessAdapter::failure_cause`,
/// on an API key as the canned provider is), as `Auth`, `RateLimit` and `Outage` respectively. A
/// harness still retrying in silence at the bound fails the cell: nothing marion reads says why.
/// The 401 run also records whether the row reads a refused credential off a frame, which is what
/// ends such a run early in marion.
fn p_errors(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-errors";
    let mut lines = Vec::new();
    let mut all_ok = true;
    for status in [401u16, 429, 500] {
        let s = c.session(P, &status.to_string());
        let marker = format!("CONFERR{status}");
        s.provider.hold.add_turn(c.turn(&marker, 0, "unreachable"));
        s.provider.hold.fault(Some(status));
        let launch = match c.compile(&s, &prompt(&marker), &Knobs::default()) {
            Ok(l) => l,
            Err(e) => {
                return Outcome::unsupported(
                    P,
                    format!("marion will not compile a canned launch: {e}"),
                );
            }
        };
        let at = Instant::now();
        let node = match Node::start(
            c.t.path,
            &launch,
            &prompt(&marker),
            Arc::clone(&s.log),
            Gate::Marion,
        ) {
            Ok(n) => n,
            Err(e) => {
                lines.push(format!("{status}: did not start: {e}"));
                continue;
            }
        };
        let ended = node.wait_ends(1, FAULT_BOUND);
        let stdout = node.proc.stdout();
        let stderr = node.proc.stderr();
        let asked = s
            .provider
            .hold
            .seen()
            .iter()
            .filter(|r| r.turn.as_ref().is_some_and(|(m, _)| *m == marker))
            .count();
        let cause =
            c.t.adapter
                .failure_cause(&stderr, &stdout, marion_harness::Billing::ApiKey);
        let failure = c.t.adapter.stream_failure(&stdout);
        let refusal = marion_harness::json_frames(&stdout)
            .iter()
            .find_map(|f| c.t.adapter.auth_refusal(f));
        // An ACP prompt's answer is its turn's end, and a refusal can be there; not so elsewhere.
        let acp_error = node
            .prompt_answers()
            .first()
            .filter(|_| c.t.path == LaunchPath::Acp)
            .map(|(_, f)| f.get("error").unwrap_or(&f["result"]).to_string());
        let exit = node.proc.with(|io| io.exit);
        let classified = matches!(
            (status, &cause),
            (401, Some(FailureCause::Auth { .. }))
                | (429, Some(FailureCause::RateLimit { .. }))
                | (500, Some(FailureCause::Outage { .. }))
        );
        all_ok &= classified;
        lines.push(format!(
            "{status}: {} after {:.1} s, {asked} request(s), {}; cause {}; stream failure \
             {:?}{}{}",
            if ended { "ended" } else { "still running" },
            at.elapsed().as_secs_f64(),
            exit_word(exit),
            match &cause {
                Some(c) => format!("{c:?}"),
                None => "none said".into(),
            },
            failure,
            match (status, &refusal) {
                (401, Some(r)) => format!("; refused credential read off a frame: {r:?}"),
                (401, None) => "; no frame reads as a refused credential".into(),
                _ => String::new(),
            },
            acp_error
                .map(|e| format!("; prompt answer {e}"))
                .unwrap_or_default(),
        ));
    }
    Outcome::judged(
        P,
        all_ok,
        "a 401 classifies as Auth, a 429 as RateLimit and a 500 as Outage, from the harness's \
         stderr and its row's error rules (the classifier marion's contracts use, API-key billing)",
        lines.join(" | "),
    )
}

// --- P-tui --------------------------------------------------------------------------------------

/// The row's pane shape in a pty: DECSET 2004 at boot, the first screen (dialogs included, since no
/// row names a first-paint needle), a bracketed paste that submits, and output going quiet once
/// the turn is done — the terminal delivery a native lane rests on.
pub fn p_tui(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-tui";
    if let Err(why) = pane_plane(c) {
        return Outcome::unsupported(P, why);
    }
    let s = c.session(P, "");
    let marker = "CONFTUI";
    s.provider
        .hold
        .add_turn(c.turn(marker, 0, "conformance tui done"));
    let knobs = Knobs {
        pane: true,
        ..Knobs::default()
    };
    let launch = match c.compile(&s, "", &knobs) {
        Ok(l) => l,
        Err(e) => {
            return Outcome::unsupported(P, format!("marion will not compile a canned pane: {e}"));
        }
    };
    let run = match pane_turn(c, &s, &launch, marker) {
        Ok(r) => r,
        Err(e) => return Outcome::judged(P, false, "", e),
    };
    let tty = &run.tty;
    paste(tty, "/mcp");
    tty.quiet(PANE_QUIET, Duration::from_secs(15));
    // Scrubbed first: the scratch path itself says `marion`.
    let mcp_screen: Vec<String> = tty.screen().iter().map(|l| s.log.scrub().text(l)).collect();
    s.log.w("note", &json!({"/mcp screen": mcp_screen}));
    let mcp_listed = mcp_screen.iter().any(|l| l.contains("marion"));
    let observed = format!(
        "DECSET 2004 at boot: {}; first screen {:?}; boot dialog {}; bracketed paste + CR \
         submitted: {}; output quiet {} ms after the turn: {}; `/mcp` screen names marion: \
         {mcp_listed}",
        run.decset,
        tail(&run.first_screen, 6),
        match run.dialog {
            None => "none of the row's on the first screen".to_string(),
            Some(d) => format!(
                "{:?} ({}) {}",
                d.needle,
                match d.answer.keys() {
                    Some(k) => format!("answered with {:?}", String::from_utf8_lossy(k)),
                    None => "held by the row".into(),
                },
                match run.then {
                    _ if run.answered => "and dismissed".to_string(),
                    Some(next) if next != d =>
                        format!("and dismissed, then {:?} ({})", next.needle, next.note),
                    _ => "and NOT dismissed".to_string(),
                }
            ),
        },
        if run.submitted {
            "on the first paste"
        } else if run.retried {
            "only on a second paste — the first-paint screen (above) swallowed the first"
        } else {
            "NO"
        },
        PANE_QUIET.as_millis(),
        run.settled,
    );
    Outcome::judged(
        P,
        run.decset && run.answered && run.submitted && run.settled,
        "bracketed paste mode at boot, any first-screen dialog is one the row names and its keys \
         dismiss, a bracketed paste + CR then submits, and the terminal goes quiet \
         (IdleSignal::OutputQuiet) once the turn is done",
        observed,
    )
}

/// How long a pane's output must be still to read as quiet (`IdleSignal::OutputQuiet`).
const PANE_QUIET: Duration = Duration::from_millis(1500);

/// The row's pane surfaces' display plane and control, or why it has none.
fn pane_plane(
    c: &Ctx<'_>,
) -> Result<(marion_harness::PtyWitness, marion_harness::ControlTransport), &'static str> {
    let Some(surfaces) = c.t.adapter.pane_surfaces() else {
        return Err("the row has no pane shape (`pane: None`)");
    };
    let Some(witness) = surfaces.display_plane() else {
        return Err("the row's pane surfaces declare no display plane");
    };
    Ok((witness, surfaces.control))
}

/// A bracketed paste, then the CR that submits it.
fn paste(tty: &crate::tty::Tty, text: &str) {
    tty.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
    std::thread::sleep(Duration::from_millis(50));
    tty.write(b"\r");
}

/// What one scripted turn in a pane did, from boot to the quiet after it ([`pane_turn`]).
struct PaneTurn {
    tty: crate::tty::Tty,
    decset: bool,
    first_screen: Vec<String>,
    dialog: Option<&'static marion_harness::spec::BootDialog>,
    /// The row's dialog on screen after the last answer, where one was.
    then: Option<&'static marion_harness::spec::BootDialog>,
    answered: bool,
    submitted: bool,
    retried: bool,
    settled: bool,
}

/// Open `launch`'s pane, walk the row's boot dialogs, paste `marker`'s prompt and wait for the
/// turn to go quiet — P-tui's drive, which every pane probe shares.
fn pane_turn(c: &Ctx<'_>, s: &Session, launch: &Launch, marker: &str) -> Result<PaneTurn, String> {
    let (witness, control) = pane_plane(c)?;
    let tty = crate::tty::Tty::spawn(witness, control, &launch.inv, Arc::clone(&s.log))
        .map_err(|e| format!("pane did not start: {e}"))?;
    let decset = tty.wait(Duration::from_secs(30), |b, _| {
        crate::tty::has(b, b"\x1b[?2004h")
    });
    // A splash that animates never goes quiet; the bound is the first paint's, not the turn's.
    let painted = tty.quiet(PANE_QUIET, Duration::from_secs(20));
    let first_screen: Vec<String> = tty.screen().iter().map(|l| s.log.scrub().text(l)).collect();
    s.log.w(
        "note",
        &json!({"first screen": first_screen, "quiet": painted}),
    );
    // The row's boot dialogs (`HarnessSpec::boot_dialogs`), read off the rendered screen: its
    // lines joined, words by one space — the reading the pty host applies to raw output. The
    // probe's scratch is a directory marion created, which is where the row's keys may answer.
    let on_screen = |lines: &[String]| {
        let text = lines
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        c.t.spec
            .boot_dialogs
            .dialogs
            .iter()
            .find(|d| text.contains(d.needle))
    };
    let dialog = on_screen(&first_screen);
    // What the screen showed after the row's keys, where it was one of the row's dialogs. A chain
    // (claude's first-run setup, then folder trust) is walked one answer per dialog, as the paste
    // driver walks it; a dialog back after its own keys stops the walk.
    let mut then = None;
    let mut seen: Vec<&str> = Vec::new();
    let mut current = dialog;
    let answered = loop {
        let Some(d) = current else { break true };
        let Some(keys) = d.answer.keys() else {
            break false;
        };
        if seen.contains(&d.needle) || seen.len() >= 4 {
            break false;
        }
        seen.push(d.needle);
        let mut before = 0;
        tty.wait(Duration::ZERO, |b, _| {
            before = b.len();
            true
        });
        tty.write(keys);
        // The redraw first, then its quiet: the first screen was already quiet.
        tty.wait(Duration::from_secs(20), |b, _| b.len() > before);
        tty.quiet(PANE_QUIET, Duration::from_secs(20));
        let after: Vec<String> = tty.screen().iter().map(|l| s.log.scrub().text(l)).collect();
        s.log.w(
            "note",
            &json!({"screen after the row's dialog keys": after}),
        );
        then = on_screen(&after);
        current = then;
    };
    let asked = |bound: Duration| {
        s.provider.hold.wait(bound, |seen, _| {
            seen.iter()
                .any(|r| r.turn.as_ref().is_some_and(|(m, _)| m == marker))
        })
    };
    paste(&tty, &prompt(marker));
    let submitted = asked(Duration::from_secs(20));
    // A dialog no row names (or one the row's keys did not dismiss) swallows the first paste; a
    // second one shows whether the composer behind it submits.
    let retried = !submitted && {
        tty.quiet(PANE_QUIET, Duration::from_secs(10));
        s.log.w(
            "note",
            &json!({"screen after an unsubmitted paste": tty.screen()}),
        );
        paste(&tty, &prompt(marker));
        asked(Duration::from_secs(30))
    };
    let settled = (submitted || retried) && tty.quiet(PANE_QUIET, TURN);
    s.log
        .w("note", &json!({"screen after the turn": tty.screen()}));
    Ok(PaneTurn {
        tty,
        decset,
        first_screen,
        dialog,
        then,
        answered,
        submitted,
        retried,
        settled,
    })
}

// --- P-untrusted-mcp ----------------------------------------------------------------------------

/// The server the probe's repository declares in `.mcp.json`, and the operator's settings approve.
const REPO_SERVER: &str = "x";

/// **A repository's `.mcp.json` server does not start on a live node in a repository the operator
/// has not trusted, even where the operator's own settings approve it by name**
/// (`enabledMcpjsonServers`), which would otherwise run a cloned repository's server of that name
/// as the operator. Headless and pane, each beside a control on the live launch of a *trusted*
/// repository, in which the same server must start: so a PASS is the untrusted launch's doing,
/// not a server that never would have started.
///
/// Probed on rows whose shapes carry [`Field::UntrustedSettings`]. Which of the untrusted rules
/// does the work is recorded, not judged: a third run removes the field's flag alone. Measured on
/// claude 2.1.302: `--setting-sources user` already keeps `.mcp.json` out (that run's server stays
/// down too), and `disabledMcpjsonServers` is what still holds where the project source loads.
///
/// Live without a login: argv is the live launch's ([`Knobs::live`]); the environment is the
/// canned launch's — its own config home (where the operator's approval is written, as a user
/// `settings.json`), the canned provider, the placeholder key. The server is a shell that touches
/// a marker in the probe's world, so "started" is a file, not a screen.
///
/// Mutation: drop the untrusted `--setting-sources user` and `disabledMcpjsonServers` both and
/// the untrusted runs start the server (FAIL); give the server nothing to start (a `.mcp.json` the
/// harness does not read) and the controls do not start it either (FAIL, never a vacuous PASS).
fn p_untrusted_mcp(c: &mut Ctx<'_>) -> Outcome {
    const P: &str = "P-untrusted-mcp";
    let shapes = [Some(c.t.spec.argv), c.t.spec.pane];
    let flag = shapes
        .iter()
        .flatten()
        .flat_map(|a| a.iter())
        .find_map(|a| match a {
            Arg::Flag(flag, Field::UntrustedSettings) => Some(*flag),
            _ => None,
        });
    let Some(flag) = flag else {
        return Outcome::unsupported(
            P,
            "the row carries no `Field::UntrustedSettings` overlay: a live node in an untrusted \
             repository has no settings layer of the operator's to outweigh",
        );
    };
    // The overlay's config home: where its seed sits, and the operator's settings beside it.
    let Some(home) =
        c.t.spec
            .overlay_documents
            .first()
            .and_then(|(rel, _)| Path::new(rel).parent())
    else {
        return Outcome::unsupported(
            P,
            "the row's overlay seeds no config home to hold the operator's approval",
        );
    };
    let mut parts = Vec::new();
    let mut pass = true;
    for (pane, shape) in [(false, c.t.spec.argv), (true, c.t.spec.pane.unwrap_or(&[]))] {
        let carries = shape
            .iter()
            .any(|a| matches!(a, Arg::Flag(f, Field::UntrustedSettings) if *f == flag));
        let name = if pane { "pane" } else { "headless" };
        if !carries || (pane && pane_plane(c).is_err()) {
            parts.push(format!(
                "{name}: the shape carries no `{flag}` overlay, not run"
            ));
            continue;
        }
        let run = |part: &str, r: Run| untrusted_run(c, &format!("{name}{part}"), home, pane, r);
        let untrusted = run("", Run::default());
        // The overlay alone: the project source back on, so `.mcp.json` is read and only the
        // overlay's rejection by name keeps the approved server down.
        let overlay = run(
            "-project-source",
            Run {
                sources: Some("user,project"),
                ..Run::default()
            },
        );
        let trusted = run(
            "-trusted",
            Run {
                trusted: true,
                approved_in_repo: true,
                ..Run::default()
            },
        );
        // A trusted repository whose server nobody approved: the pane must neither stall on the
        // approval dialog nor start it. Recorded only, headless.
        let unapproved = run(
            "-trusted-unapproved",
            Run {
                trusted: true,
                approved_by_user: false,
                ..Run::default()
            },
        );
        let bare = run(
            &format!("-no-{}", flag.trim_start_matches('-')),
            Run {
                strip: Some(flag),
                ..Run::default()
            },
        );
        let word = |r: &Result<Started, String>| match r {
            Ok(r) => format!("server started {} (turn {})", r.started, r.turned),
            Err(e) => e.clone(),
        };
        let down = |r: &Result<Started, String>| matches!(r, Ok(r) if !r.started && r.turned);
        let ok = down(&untrusted)
            && down(&overlay)
            && matches!(&trusted, Ok(t) if t.started)
            && (!pane || down(&unapproved));
        pass &= ok;
        parts.push(format!(
            "{name}: untrusted: {}; untrusted with `--setting-sources user,project` (the overlay \
             alone): {}; trusted, approved (control): {}; trusted, unapproved ({}): {}; untrusted \
             less `{flag}` (recorded, not judged): {}",
            word(&untrusted),
            word(&overlay),
            word(&trusted),
            if pane { "judged" } else { "recorded" },
            word(&unapproved),
            word(&bare)
        ));
    }
    Outcome::judged(
        P,
        pass,
        "on a live launch in an untrusted repository, a `.mcp.json` server the operator's \
         settings approve by name (`enabledMcpjsonServers`) does not start, headless or in the \
         pane, also with the project setting source on (the overlay alone); the same server starts \
         on the live launch of a trusted repository whose settings approve it; and a trusted \
         repository's unapproved server neither stalls a pane nor starts in it",
        parts.join(" | "),
    )
}

/// How one [`untrusted_run`] departs from the untrusted live launch with the operator's approval.
struct Run<'a> {
    /// The repository is trusted in marion.
    trusted: bool,
    /// A flag (with its value) taken off the compiled argv.
    strip: Option<&'a str>,
    /// `--setting-sources`' value in place of the compiled one.
    sources: Option<&'a str>,
    /// The operator's user settings approve the server by name.
    approved_by_user: bool,
    /// The repository's own `.claude/settings.local.json` approves it too.
    approved_in_repo: bool,
}

impl Default for Run<'_> {
    fn default() -> Self {
        Run {
            trusted: false,
            strip: None,
            sources: None,
            approved_by_user: true,
            approved_in_repo: false,
        }
    }
}

/// What one [`p_untrusted_mcp`] run saw.
struct Started {
    /// The repository's server touched its marker.
    started: bool,
    /// The scripted turn reached the provider: the node got as far as a turn.
    turned: bool,
}

/// One run of [`p_untrusted_mcp`]: the repository's server, the approvals `r` names, the live
/// launch of a repository trusted or not as `r` says, one scripted turn.
fn untrusted_run(
    c: &Ctx<'_>,
    part: &str,
    home: &Path,
    pane: bool,
    r: Run<'_>,
) -> Result<Started, String> {
    let s = c.session("P-untrusted-mcp", part);
    let marker_file = s.world.config.join("repo-server-started");
    // Planted before the compile: the launch reads the server names it rejects off this file.
    let server = json!({"mcpServers": {REPO_SERVER: {"command": "/bin/sh", "args": ["-c",
        format!("touch '{}'; sleep 5", marker_file.display())]}}});
    std::fs::write(s.world.repo.join(".mcp.json"), server.to_string())
        .map_err(|e| format!(".mcp.json: {e}"))?;
    let approval = json!({"enabledMcpjsonServers": [REPO_SERVER]}).to_string();
    if r.approved_in_repo {
        let dir = s.world.repo.join(".claude");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(dir.join("settings.local.json"), &approval)
            .map_err(|e| format!("settings.local.json: {e}"))?;
    }
    let marker = "CONFREPOMCP";
    s.provider
        .hold
        .add_turn(c.turn(marker, 0, "conformance repo mcp done"));
    let knobs = Knobs {
        pane,
        live: Some(r.trusted),
        ..Knobs::default()
    };
    let text = if pane { String::new() } else { prompt(marker) };
    let mut launch = c
        .compile(&s, &text, &knobs)
        .map_err(|e| format!("marion will not compile the live launch: {e}"))?;
    if let Some(flag) = r.strip {
        strip_flag(&mut launch.inv.args, flag, true);
    }
    if let Some(sources) = r.sources {
        let args = &mut launch.inv.args;
        let at = args
            .iter()
            .position(|a| a == "--setting-sources")
            .ok_or("the live argv carries no `--setting-sources` to widen")?;
        args[at + 1] = sources.to_string();
    }
    // The operator's approval of the repository's server, in the user settings the launch reads.
    let dir = s.world.config.join(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if r.approved_by_user {
        std::fs::write(dir.join("settings.json"), &approval)
            .map_err(|e| format!("settings.json: {e}"))?;
    }
    s.log.w(
        "note",
        &json!({"argv": launch.inv.args, "repo .mcp.json": server}),
    );
    let turned = if pane {
        let run = pane_turn(c, &s, &launch, marker)?;
        run.submitted || run.retried
    } else {
        let node = Node::start(
            c.t.path,
            &launch,
            &prompt(marker),
            Arc::clone(&s.log),
            Gate::Marion,
        )?;
        node.wait_ends(1, TURN)
    };
    let started = marker_file.exists();
    s.log.w(
        "note",
        &json!({"repo server started": started, "turn": turned}),
    );
    Ok(Started { started, turned })
}

/// The last `n` non-blank lines of a screen — where a dialog's question and options sit.
fn tail(screen: &[String], n: usize) -> Vec<&str> {
    let lines: Vec<&str> = screen
        .iter()
        .map(String::as_str)
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Where a harness's fixtures go: `<out>/<selector>-<version>/`, selector made path-safe.
pub fn fixture_dir(out: &Path, selector: &str, version: &str) -> PathBuf {
    // An ACP agent's version is its handshake's `<name> <version>`; the directory takes the number.
    let v = version.split_whitespace().last().unwrap_or("unknown");
    out.join(format!("{}-{v}", selector.replace(':', "-")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P-activity's expected figure: every counted request, or — for a row that says its figure
    /// leaves the rest out — only those that offered tools, wherever they fall in the run.
    #[test]
    fn the_expected_usage_adds_every_request_or_only_the_tool_offering_ones() {
        let side = |counts| Sent {
            counts,
            offers_tools: false,
        };
        let turn = |counts| Sent {
            counts,
            offers_tools: true,
        };
        let sent = [turn((211, 17, 10)), side((111, 10, 5)), turn((311, 24, 15))];
        let all = sent_total(&sent, false);
        assert_eq!((all.input, all.output, all.cache_read), (633, 51, 30));
        let turn_only = sent_total(&sent, true);
        assert_eq!(
            (turn_only.input, turn_only.output, turn_only.cache_read),
            (522, 41, 25)
        );
        let reported = TokenUsage {
            input: 522,
            output: 41,
            cache_read: 25,
            reasoning: Some(0),
            ..TokenUsage::default()
        };
        assert!(
            same_counters(&reported, &turn_only),
            "reasoning is not compared"
        );
        assert!(!same_counters(&reported, &all));
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// **An ask that gained or lost a key against the verified release's transcript is drift**;
    /// the same keys with other values are not, and nothing recorded is nothing to compare.
    #[test]
    fn an_ask_that_moved_against_the_verified_transcript_is_named() {
        let dir = marion_testsupport::scratch("conformance-ask-shape");
        let ask = |extra: Value| {
            let mut request = json!({"subtype": "can_use_tool", "tool_name": "mcp__marion__report",
                "permission_suggestions": [{"type": "addRules"}]});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"type": "control_request", "request_id": "r", "request": request})
        };
        assert_eq!(
            ask_shape_drift(&dir, &[ask(json!({}))]),
            None,
            "no transcript"
        );
        let line = json!({"dir": "s2c", "msg": ask(json!({"mcp_server": {"name": "marion"}}))});
        std::fs::write(dir.join("p-approval-ungranted.jsonl"), format!("{line}\n")).unwrap();
        std::fs::write(
            dir.join("p-launch.jsonl"),
            format!("{}\n", json!({"msg": ask(json!({"other": 1}))})),
        )
        .unwrap();

        let same = ask(json!({"mcp_server": {"name": "another"}}));
        assert_eq!(ask_shape_drift(&dir, &[same]), None, "values may move");
        let moved = ask(json!({"server_policy_ask_only": true}));
        assert_eq!(
            ask_shape_drift(&dir, &[moved]).as_deref(),
            Some("+request.server_policy_ask_only, -request.mcp_server, -request.mcp_server.name")
        );
        assert_eq!(
            ask_shape_drift(&dir, &[]),
            None,
            "nothing asked: P-approval judges that"
        );
    }

    #[test]
    fn stripping_a_grant_removes_the_flag_and_only_its_own_value() {
        let mut a = args(&["-p", "--allowedTools", "mcp__marion__report", "--yolo", "x"]);
        strip_flag(&mut a, "--allowedTools", true);
        assert_eq!(a, args(&["-p", "--yolo", "x"]));
        strip_flag(&mut a, "--yolo", false);
        assert_eq!(a, args(&["-p", "x"]));
        let mut b = args(&["--allow-tool=marion(report)", "-p"]);
        strip_flag(&mut b, "--allow-tool", true);
        assert_eq!(b, args(&["-p"]));
    }

    /// A TOML contest replaces the top-level line that set its key and goes above the first
    /// table; a key of the same name inside a table is left alone.
    #[test]
    fn a_toml_contest_replaces_the_top_level_key_and_nothing_under_a_table() {
        let doc = "model = \"m\"\napproval_policy = \"never\"\n\n[t]\napproval_policy = \"x\"";
        assert_eq!(
            contest_toml(doc, r#"approval_policy = "on-request""#),
            "approval_policy = \"on-request\"\nmodel = \"m\"\n\n[t]\napproval_policy = \"x\""
        );
    }

    #[test]
    fn stripping_a_declaration_key_reaches_documents_and_override_pairs() {
        let dir = marion_testsupport::scratch("conf-strip");
        let json_doc = dir.join("a.json");
        let toml_doc = dir.join("b.toml");
        std::fs::write(
            &json_doc,
            r#"{"mcpServers":{"marion":{"trust":true,"command":"m"}}}"#,
        )
        .unwrap();
        std::fs::write(
            &toml_doc,
            "[mcp_servers.marion]\ntrust = true\ncommand = \"m\"\n",
        )
        .unwrap();
        let mut l = Launch {
            inv: marion_harness::invocation::Invocation {
                sandbox: None,
                inherit: None,
                program: "h".into(),
                args: args(&["exec", "-c", "mcp_servers.marion.trust=true", "-c", "x=1"]),
                env: vec![],
                cwd: dir.to_path_buf(),
                model: None,
                session_mode: None,
                env_remove: vec![],
            },
            files: vec![json_doc.clone(), toml_doc.clone()],
            session: None,
            ready_file: None,
            rpc: None,
            dialect: marion_supervisor::duplex::Dialect::StreamJson,
        };
        strip_key(&mut l, "trust");
        assert_eq!(l.inv.args, args(&["exec", "-c", "x=1"]));
        assert!(
            !std::fs::read_to_string(&json_doc)
                .unwrap()
                .contains("trust")
        );
        let toml = std::fs::read_to_string(&toml_doc).unwrap();
        assert!(
            !toml.contains("trust") && toml.contains("command"),
            "{toml}"
        );
    }
}
