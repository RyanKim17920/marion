//! `marion doctor` — §8's probe, and §3.3's static capability table (*"the same code as runtime
//! resolution"*).
//!
//! > §8: *"**`marion doctor`** probes each installed harness and produces the static capability
//! > table (§3.3) […] It has **two modes**, and the second matters more: `--capabilities` asks what
//! > a harness *advertises*; **`--adapter` runs a micro-contract test** (spawn → prompt → assert
//! > response shape → interrupt → assert clean termination → kill *if it is still alive* […])
//! > against the installed binary. Feature flags drift less than behavior does, and every
//! > retraction in §12 was a behavioral surprise, not a missing capability."*
//!
//! Until this module existed `marion-supervisor doctor` printed `"no adapters registered yet (M1 in
//! progress)"` — four milestones after M1 — and `MILESTONES.md` listed it under what does not
//! exist.
//!
//! # The key, and why it is not `(harness, version)`
//!
//! §3.3 keys the static table `(harness, harness_version, surfaces)` and §9's M1 note is emphatic
//! about the third component: doctor *"never publishes 'codex cannot resume'; it publishes 'codex
//! **on this surface** cannot'"*. Every row printed here therefore names its surfaces, and the
//! capability column is [`marion_harness::static_caps`] on exactly that key — the same function the
//! runtime resolution calls, not a parallel table that agrees with it today.
//!
//! A key with three components that only ever varies two of them is a key in name only, so **a
//! harness with a pane surface produces two rows**, one per [`SurfaceRole`], and not one row with a
//! footnote about the other. The difference is measured, not decorative: claude-code advertises
//! `interrupt` and `permissions`, and on its node surface — `Typed(StreamJson)` — both publish,
//! while on its pane surface — §3.4's `opaque`, where §3.4 says marion *"can write, but cannot
//! address a turn"* — `permissions` is clipped and `interrupt` survives. Collapsing those two into
//! one row is exactly the sentence §9's M1 forbids, said about claude-code instead of codex.
//!
//! # What `--adapter` actually runs, and what it refuses by name
//!
//! Each step below reports itself, including the ones that do not run. §8's whole argument for this
//! mode is that a probe says *why* a harness is unusable — *"a version too old and a binary that
//! hangs on interrupt are the same boolean and different problems"* — so a step that is skipped
//! must say so in the same place a step that failed would, and never by being absent.
//!
//! Two steps deliberately never run, and both are refusals rather than omissions:
//!
//! * **Keystroke-injection submit.** §8 says the probe *"**must** include"* it and calls it *"the
//!   most version-fragile mechanism"*. It is not implemented and is not implementable here: §8's
//!   own L7 row assigns it to a real terminal (*"A pty host can send bytes; only a real terminal
//!   settles whether a harness's submit handling agrees"*), and this probe has a pipe. Every
//!   `--adapter` report says so on its own line.
//! * **The live turn on a typed control plane.** claude-code's surfaces are `Typed(StreamJson)`, so
//!   §6.1 step 8 writes its prompt as a frame *after* a readiness gate rather than into argv. That
//!   is `duplex`'s protocol, not a probe's, and driving half of it here would test marion's idea of
//!   the handshake rather than the handshake. Reported, not attempted.
//!
//! # A live turn costs a model call, and marion may not choose a model
//!
//! `--adapter`'s spawn step runs the real binary against the operator's real credential, which is
//! what §8 asks for (*"Run `--adapter` in CI"*). Two adapters — gemini and opencode — make an
//! explicit model a MUST at `compile`, and marion has no basis for picking one (S12 measured 0.53.0
//! rewriting even an explicit `-m`). So those two report the spawn step as **not run for want of a
//! model** unless `--model` names one, rather than being handed a guess.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marion_core::encoding::Millis;
use marion_core::harness::Harness;
use marion_harness::{
    Auth, Capabilities, ExecutionSurfaces, Extras, HarnessAdapter, Invocation, LaunchSpec,
    McpDeclaration, SpawnCtx, adapter_for, static_caps,
};
use marion_proto::{HarnessReport, ProbeMode};

/// The micro-contract's prompt. Short, deterministic to check, and cheap: the assertion §8 asks for
/// is a *response shape*, so the content only has to be something a model will answer at all.
const MICRO_PROMPT: &str = "Reply with exactly the word: marion. Do not use any tools.";

/// How long the spawn step waits for the run to finish before it interrupts it. A bound, not an
/// expectation — §8's step is *"kill if it is still alive"*.
const TURN_BUDGET: Duration = Duration::from_secs(90);
/// How long the interrupt is given to produce a clean termination before the leak check escalates.
/// S16 measured the supervisor's own ladder at SIGINT → SIGTERM 100 ms → SIGKILL ~450 ms; this is
/// deliberately far looser, because a slow-but-clean shutdown is a *finding*, not a failure.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
/// `<program> --version` is a local read; anything slower than this is itself the finding.
const VERSION_BUDGET: Duration = Duration::from_secs(20);

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub mode: ProbeMode,
    /// `None` probes every harness in [`Harness::ALL`]. §8's filter *is* performed: `--adapter`
    /// spawns a real process per harness, so probing one instead of four is a difference the
    /// operator can observe on their bill.
    pub harness: Option<Harness>,
    /// The model `--adapter`'s live turn runs against, where the adapter requires one. Never
    /// defaulted (§6.4).
    pub model: Option<String>,
}

/// Which of a harness's surfaces a row is keyed on.
///
/// Not a cosmetic label: it is the third component of §3.3's key made enumerable, so that "two
/// rows for one harness" is a structure rather than a coincidence of two adapters happening to
/// answer [`HarnessAdapter::pane_surfaces`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
    /// What [`HarnessAdapter::surfaces`] returns — the shape every run gets unless it asks for a
    /// pane.
    Node,
    /// What [`HarnessAdapter::pane_surfaces`] returns, where it returns anything.
    Pane,
}

impl SurfaceRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Pane => "pane",
        }
    }
}

/// One `(harness, version, surfaces)` row.
///
/// [`HarnessReport`] carries the parts that are already the wire vocabulary; `surfaces` and
/// `capabilities` sit beside it rather than inside it because `marion-proto` *deliberately* does
/// not depend on `marion-harness` — see that crate's manifest, which refuses the dependency by
/// name so a protocol crate does not pull in four adapters to name a boolean.
#[derive(Debug, Clone)]
pub struct Row {
    pub report: HarnessReport,
    pub role: SurfaceRole,
    pub surfaces: ExecutionSurfaces,
    pub capabilities: Capabilities,
}

/// Parse `doctor`'s argv (everything after the subcommand).
///
/// Unknown flags are an error rather than an ignore. §8's neighbours in the design doc make the
/// same point about configuration — *"Unknown keys are a load error surfaced by `marion doctor`,
/// not silently ignored"* — and a doctor that silently ignored its own is not in a position to say
/// it.
pub fn parse_args(argv: &[String]) -> Result<Options, String> {
    let mut mode = None;
    let mut harness = None;
    let mut model = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--capabilities" => mode = Some(ProbeMode::Capabilities),
            "--adapter" => mode = Some(ProbeMode::Adapter),
            "--harness" => {
                let v = it.next().ok_or("--harness needs a harness name")?;
                harness = Some(v.parse::<Harness>().map_err(|e| e.to_string())?);
            }
            "--model" => {
                model = Some(it.next().ok_or("--model needs a model id")?.clone());
            }
            other => return Err(format!("unknown doctor flag `{other}`")),
        }
    }
    Ok(Options {
        // §8 names `--capabilities` first and calls the other the one that matters more. Neither is
        // the default: they cost differently by orders of magnitude — one reads a version string,
        // the other spawns an agent and spends a model call — so choosing for the operator is
        // choosing how much their `doctor` costs.
        mode: mode.ok_or("marion doctor needs a mode: --capabilities or --adapter")?,
        harness,
        model,
    })
}

/// Probe, in the order [`Harness::ALL`] declares — one row per `(harness, version, surfaces)` key,
/// so a harness with a pane surface contributes two.
pub fn run(opts: &Options) -> Vec<Row> {
    Harness::ALL
        .into_iter()
        .filter(|h| opts.harness.is_none_or(|only| only == *h))
        .flat_map(|h| probe(h, opts))
        .collect()
}

fn probe(h: Harness, opts: &Options) -> Vec<Row> {
    let started = Instant::now();
    let mut notes = Vec::new();

    let adapter = match adapter_for(h) {
        Ok(a) => a,
        Err(e) => {
            return vec![Row {
                report: HarnessReport {
                    harness: h,
                    harness_version: None,
                    adapter_check: matches!(opts.mode, ProbeMode::Adapter).then_some(false),
                    notes: vec![format!("adapter: {e}")],
                    elapsed: Millis(started.elapsed()),
                },
                role: SurfaceRole::Node,
                surfaces: ExecutionSurfaces::opaque(),
                capabilities: Capabilities::NONE,
            }];
        }
    };
    let surfaces = adapter.surfaces();

    // The binary the adapter would actually launch, read off a compiled `Invocation` rather than
    // from a table of names here. A second list of program names is a second thing to keep true.
    if opts.model.is_none() {
        notes.push(format!(
            "model: none given, so argv is compiled with the placeholder `{PLACEHOLDER_MODEL}` \
             for the steps that launch nothing. No process is ever started from it."
        ));
    }
    let program = probe_spec(opts.model.clone(), McpDeclaration::None)
        .and_then(|spec| adapter.compile(&spec, &probe_ctx()).ok())
        .map(|inv| inv.program);
    let resolved = match &program {
        Some(p) => match which(p) {
            Some(path) => {
                notes.push(format!("binary: {} ({})", p, path.display()));
                Some(path)
            }
            None => {
                notes.push(format!("binary: `{p}` not found on $PATH"));
                None
            }
        },
        None => {
            notes.push(
                "binary: this adapter refused to compile an invocation at all, so marion cannot \
                 name the program it would launch"
                    .into(),
            );
            None
        }
    };

    let version = resolved
        .as_deref()
        .and_then(|p| read_version(p, &mut notes));

    let adapter_check = match opts.mode {
        ProbeMode::Capabilities => None,
        ProbeMode::Adapter => Some(micro_contract(
            &*adapter,
            resolved.as_deref(),
            opts,
            &mut notes,
        )),
    };

    // One row per surface key. Everything above is a fact about the *binary* — its path, its
    // version, whether the micro-contract passed — and so is shared; everything below is a fact
    // about the binary *at a key*, and is not.
    let mut keys = vec![(SurfaceRole::Node, surfaces)];
    if let Some(pane) = adapter.pane_surfaces() {
        keys.push((SurfaceRole::Pane, pane));
    }
    let elapsed = Millis(started.elapsed());
    keys.into_iter()
        .map(|(role, s)| {
            let mut notes = notes.clone();
            notes.push(format!(
                "surfaces ({}): control={:?} display={:?} observations={:?}",
                role.as_str(),
                s.control,
                s.display,
                s.observations
            ));
            // §3.3's middle third.
            let capabilities = static_caps(h, keyed_version(version.as_deref()), &s);
            notes.push(format!(
                "capabilities on these surfaces: {}",
                match capabilities.granted().as_slice() {
                    [] => "none".to_string(),
                    g => g.join(", "),
                }
            ));
            let unmeasured: Vec<&str> = Capabilities::FIELDS
                .iter()
                .copied()
                .filter(|f| !capabilities.granted().contains(f))
                .collect();
            notes.push(format!(
                "not published here: {} — a false is \"marion has not measured it on this \
                 surface\", never \"the harness cannot\"",
                unmeasured.join(", ")
            ));
            if role == SurfaceRole::Pane && adapter_check.is_some() {
                notes.push(
                    "adapter check: NOT RUN on this surface — §8's micro-contract ran against the \
                     `node` row's invocation, and `compile_pane` produces different argv. This \
                     row's capabilities are the same binary at a different key, not a second \
                     contract test."
                        .into(),
                );
            }
            Row {
                report: HarnessReport {
                    harness: h,
                    harness_version: version.clone(),
                    // §8's micro-contract exercises the invocation the *node* row names, so only
                    // that row carries its verdict. Copying it onto the pane row would report a
                    // check as having run against argv it never saw — `compile_pane` produces
                    // different argv, and on claude-code a different control plane entirely.
                    adapter_check: match role {
                        SurfaceRole::Node => adapter_check,
                        SurfaceRole::Pane => None,
                    },
                    notes,
                    elapsed,
                },
                role,
                surfaces: s,
                capabilities,
            }
        })
        .collect()
}

/// §8's micro-contract. Returns whether every step that **ran** passed; each step that did not run
/// says so in `notes` and does not decide the boolean either way.
fn micro_contract(
    adapter: &dyn HarnessAdapter,
    program: Option<&Path>,
    opts: &Options,
    notes: &mut Vec<String>,
) -> bool {
    let mut ok = true;
    let mut step = |passed: bool, line: String, notes: &mut Vec<String>| {
        ok &= passed;
        notes.push(line);
    };

    // §8 requires it and this probe cannot do it. Named on every report, in the same place a
    // failure would be named, so it can never read as "checked and fine".
    notes.push(
        "keystroke-injection submit: NOT RUN — §8 requires this check and calls it the most \
         version-fragile mechanism; it needs a real terminal (§8's L7 row), and this probe has a \
         pipe. Not implemented, not inferred, not passed."
            .into(),
    );

    let Some(program) = program else {
        notes.push(
            "micro-contract: NOT RUN — no binary to run it against (see `binary:` above)".into(),
        );
        return false;
    };

    // Step: the declaration route the adapter promises is actually taken. No process, no model
    // call, and it catches §6.1 step 8's failure class — an adapter that forgets its declaration
    // and launches a node with no bridge at all.
    match probe_spec(opts.model.clone(), McpDeclaration::Marion) {
        None => notes.push("declaration: NOT RUN — no spec could be built".into()),
        Some(spec) => {
            let ctx = probe_ctx();
            let compiled = adapter
                .config_files(&spec, &ctx)
                .and_then(|files| Ok((files, adapter.compile(&spec, &ctx)?)));
            match compiled {
                Err(e) => step(false, format!("declaration: FAILED — compile: {e}"), notes),
                Ok((files, inv)) => {
                    let written: Vec<PathBuf> = files.into_iter().map(|(p, _)| p).collect();
                    match adapter.mcp_route(&spec).verify(&written, &inv) {
                        Ok(_) => step(
                            true,
                            format!(
                                "declaration: ok — marion's MCP declaration is present on the \
                                 route this adapter states ({:?})",
                                adapter.mcp_route(&spec)
                            ),
                            notes,
                        ),
                        Err(route) => step(
                            false,
                            format!(
                                "declaration: FAILED — the adapter promised {route} and the \
                                 compiled launch does not carry it; a node launched this way \
                                 would have no bridge"
                            ),
                            notes,
                        ),
                    }
                }
            }
        }
    }

    // Step: the live turn. Only where the prompt rides argv — see this module's header for why a
    // typed plane's post-launch prompt frame is `duplex`'s protocol and not a probe's.
    if adapter.surfaces().has_typed_control_plane() {
        notes.push(format!(
            "live turn: NOT RUN — {} runs on a typed control plane, where §6.1 step 8 writes the \
             prompt as a frame after a readiness gate rather than into argv. Driving that here \
             would test marion's idea of the handshake, not the handshake.",
            adapter.harness()
        ));
        return ok;
    }
    let Some(spec) = probe_spec_with_prompt(opts.model.clone()) else {
        notes.push(
            "live turn: NOT RUN — this adapter makes an explicit model a MUST at compile and \
             marion may not choose one for the operator (§6.4; S12 measured 0.53.0 rewriting even \
             an explicit `-m`). Pass --model to run it."
                .into(),
        );
        return ok;
    };
    let inv = match adapter.compile(&spec, &probe_ctx()) {
        Ok(i) => i,
        Err(e) => {
            step(false, format!("live turn: FAILED — compile: {e}"), notes);
            return ok;
        }
    };
    let turn = live_turn(adapter, program, &inv, notes);
    step(turn.spawned, turn.spawn_line, notes);
    if let Some(l) = turn.shape_line {
        step(turn.shape_ok, l, notes);
    }
    for l in turn.trailing {
        notes.push(l);
    }
    step(turn.no_leak, turn.leak_line, notes);
    ok
}

struct TurnOutcome {
    spawned: bool,
    spawn_line: String,
    shape_ok: bool,
    shape_line: Option<String>,
    trailing: Vec<String>,
    no_leak: bool,
    leak_line: String,
}

/// spawn → prompt → response shape → interrupt → clean termination → leak check, in §8's order and
/// with §8's caveat: the interrupt is *conditional on the child still being there*, because a
/// clean run usually is not.
fn live_turn(
    adapter: &dyn HarnessAdapter,
    program: &Path,
    inv: &Invocation,
    notes: &mut Vec<String>,
) -> TurnOutcome {
    notes.push(format!(
        "live turn: {} {}",
        program.display(),
        inv.args.join(" ")
    ));
    let mut cmd = Command::new(program);
    cmd.args(&inv.args)
        .current_dir(&inv.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &inv.env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return TurnOutcome {
                spawned: false,
                spawn_line: format!("spawn: FAILED — {e}"),
                shape_ok: false,
                shape_line: None,
                trailing: vec![],
                // Nothing was started, so nothing can have leaked. Reporting a leak here would be
                // a second failure invented from the first.
                no_leak: true,
                leak_line: "leak check: nothing was started".into(),
            };
        }
    };
    let pid = child.id() as i32;

    let (out, timed_out) = read_bounded(&mut child, TURN_BUDGET);
    let mut trailing = Vec::new();

    // §8: "kill *if it is still alive*, since a clean termination means it usually is not — the
    // step is a leak check, not a sequenced expectation".
    let interrupted = match child.try_wait() {
        Ok(Some(_)) => {
            trailing.push(
                "interrupt: not needed — the run terminated on its own before the budget".into(),
            );
            false
        }
        _ => {
            unsafe { kill(pid, SIGINT) };
            trailing.push(format!(
                "interrupt: SIGINT sent{}",
                if timed_out {
                    " after the turn budget elapsed"
                } else {
                    ""
                }
            ));
            true
        }
    };

    let exit = wait_bounded(&mut child, INTERRUPT_GRACE);
    match &exit {
        Some(s) => trailing.push(format!(
            "termination: exit {}{}",
            s.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signalled".into()),
            if interrupted {
                " after SIGINT"
            } else {
                " clean"
            }
        )),
        None => trailing.push(format!(
            "termination: FAILED — still running {INTERRUPT_GRACE:?} after SIGINT"
        )),
    }

    // The leak check, and it is unconditional: an ACP-style long-lived stdio agent that ignores
    // SIGINT is exactly the shape this exists to catch, and a probe that leaves one behind has
    // done more harm than the check was worth.
    let (no_leak, leak_line) = if exit.is_some() {
        (true, "leak check: no process outlived the probe".into())
    } else {
        unsafe { kill(pid, SIGKILL) };
        let _ = child.wait();
        (
            false,
            format!(
                "leak check: FAILED — pid {pid} survived SIGINT and had to be SIGKILLed. §8: a \
                 binary that hangs on interrupt and a version too old are the same boolean and \
                 different problems; this is the former."
            ),
        )
    };

    let shape = shape_finding(adapter, &out, exit, interrupted, timed_out);
    TurnOutcome {
        spawned: true,
        spawn_line: format!("spawn: ok — pid {pid}"),
        shape_ok: shape.0,
        shape_line: Some(shape.1),
        trailing,
        no_leak,
        leak_line,
    }
}

/// §8's *"assert response shape"*, through the adapter's **own** reader.
///
/// Not a substring match on the model's words: §8's L5 rule is *"structural assertions only, never
/// on text"*, and the thing under test is whether `parse_stream` — the function the supervisor uses
/// to read every child of this harness — can still read what this binary emits. A stream that
/// parses to a shaped outcome passes; one that yields nothing at all is the format drift §8 names
/// as the whole reason this mode exists.
fn shape_finding(
    adapter: &dyn HarnessAdapter,
    stdout: &str,
    exit: Option<std::process::ExitStatus>,
    interrupted: bool,
    timed_out: bool,
) -> (bool, String) {
    if stdout.trim().is_empty() {
        return (
            false,
            "response shape: FAILED — the binary wrote nothing to stdout".into(),
        );
    }
    let frames = marion_harness::json_frames(stdout).len();
    if frames == 0 {
        return (
            false,
            format!(
                "response shape: FAILED — {} bytes of stdout and not one JSON frame in it; this \
                 is the format drift §8 says this mode exists to catch",
                stdout.len()
            ),
        );
    }
    // Through the adapter's own reader, which is the function the supervisor uses to read every
    // child of this harness. A stream this cannot read is one whose contract has drifted, however
    // well-formed its JSON is.
    let outcome = adapter.parse_stream(
        stdout,
        marion_harness::ChildExit {
            code: exit.and_then(|s| s.code()),
            signal: None,
            timed_out,
        },
    );
    let read_something = outcome.narrative.is_some()
        || outcome.failure.is_some()
        || !outcome.file_change_paths.is_empty()
        || !outcome.result_commits.is_empty();
    if !read_something && !interrupted {
        return (
            false,
            format!(
                "response shape: FAILED — {frames} JSON frames, and \
                 `{}`'s own `parse_stream` found neither a narrative nor a failure in them",
                adapter.harness()
            ),
        );
    }
    (
        true,
        format!(
            "response shape: ok — {frames} JSON frames, read by {}'s own `parse_stream`{}",
            adapter.harness(),
            match &outcome.failure {
                Some(f) => format!(" (the harness reported a failure: {f})"),
                None => String::new(),
            }
        ),
    )
}

/// Read a child's stdout to EOF, or until `budget`. `true` means the budget elapsed first.
fn read_bounded(child: &mut std::process::Child, budget: Duration) -> (String, bool) {
    use std::io::Read;
    let Some(mut out) = child.stdout.take() else {
        return (String::new(), false);
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // Detached deliberately: if the read blocks past the budget the caller is about to SIGINT and
    // then SIGKILL the child, which closes the pipe and lets this thread finish on its own.
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    match rx.recv_timeout(budget) {
        Ok(s) => (s, false),
        Err(_) => (String::new(), true),
    }
}

fn wait_bounded(
    child: &mut std::process::Child,
    budget: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `<program> --version`, and the version token out of it.
fn read_version(program: &Path, notes: &mut Vec<String>) -> Option<String> {
    let started = Instant::now();
    let mut child = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| notes.push(format!("version: FAILED — {e}")))
        .ok()?;
    let (out, timed_out) = read_bounded(&mut child, VERSION_BUDGET);
    if timed_out {
        unsafe { kill(child.id() as i32, SIGKILL) };
        let _ = child.wait();
        notes.push(format!(
            "version: FAILED — `--version` did not answer within {VERSION_BUDGET:?}"
        ));
        return None;
    }
    let _ = child.wait();
    match version_token(&out) {
        Some(v) => {
            notes.push(format!(
                "version: {v} (read in {} ms)",
                started.elapsed().as_millis()
            ));
            Some(v)
        }
        None => {
            notes.push(format!(
                "version: FAILED — no `major.minor.patch` in {:?}",
                out.lines().next().unwrap_or("").trim()
            ));
            None
        }
    }
}

/// What §3.3's middle key component carries when `--version` could not be read.
///
/// A named function rather than an inline `unwrap_or`, because the *direction* of the fallback is
/// the decision and it deserves somewhere a test can point at. An unread version keys as the empty
/// string, which [`marion_harness::advertised`] cannot parse and therefore treats as **below every
/// measured floor**. Keying it as latest — or as the newest version marion happens to know about —
/// would publish capabilities off a version nobody read, which is precisely the guess §3.3's
/// *degrade visibly* rule forbids.
fn keyed_version(read: Option<&str>) -> &str {
    read.unwrap_or("")
}

/// The first `major.minor.patch` in a `--version` line.
///
/// A scan rather than "the whole first line", because the four binaries do not agree on the shape:
/// some print a bare `0.147.0` and some prefix it with their own name. `advertised` compares this
/// numerically, so a value it cannot parse must not reach it.
pub fn version_token(out: &str) -> Option<String> {
    out.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .map(|t| t.trim_start_matches('v'))
        .find(|t| {
            let mut parts = t.split('.');
            let ok = |maybe: Option<&str>| {
                maybe.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            };
            ok(parts.next()) && ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
        })
        .map(str::to_string)
}

/// First executable match on `$PATH`, with §7.7's symlink resolution applied.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = Path::new(program);
        return is_executable(p).then(|| std::fs::canonicalize(p).unwrap_or_else(|_| p.into()));
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(program))
        .find(|p| is_executable(p))
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The model a spec that **launches nothing** is compiled with, where the operator named none.
///
/// gemini and opencode refuse to compile without one, and marion may not choose a real model for
/// the operator (§6.4; S12 measured 0.53.0 rewriting even an explicit `-m`). But the two things
/// this spec is used for — reading the program name off the compiled argv, and checking the MCP
/// declaration route was taken — do not depend on the model at all, and a probe that reported
/// "gemini: unknown, pass --model" for a *version string* would be refusing a question it can
/// answer.
///
/// It is a placeholder in the strict sense: no process is ever started from a spec carrying it.
/// The one step that does start a process, the live turn, does **not** accept it — see
/// [`probe_spec_with_prompt`].
const PLACEHOLDER_MODEL: &str = "marion-doctor/no-model-chosen";

/// A spec that compiles but launches nothing — for reading a program name and checking a
/// declaration route.
fn probe_spec(model: Option<String>, mcp: McpDeclaration) -> Option<LaunchSpec> {
    Some(LaunchSpec {
        cwd: std::env::temp_dir(),
        model: Some(model.unwrap_or_else(|| PLACEHOLDER_MODEL.to_string())),
        prompt: String::new(),
        tools: vec![],
        allowed_tools: vec![],
        mcp,
        base_url: None,
        api_key: None,
        // §6.4: the operator's own credential, never a canned endpoint. §8's point is a contract
        // test against *the installed binary as installed*.
        auth: Auth::Inherited,
        config_dir: std::env::temp_dir().join("marion-doctor"),
        extra: Extras::default(),
    })
}

/// The same spec carrying the micro-prompt in argv. `None` where the harness needs a model marion
/// has not been given — checked here rather than at `compile`, so the refusal reads as a *reason*
/// instead of as a `HarnessError` an operator has to interpret.
fn probe_spec_with_prompt(model: Option<String>) -> Option<LaunchSpec> {
    // Deliberately **not** `PLACEHOLDER_MODEL`: this is the one spec a process is started from.
    let mut spec = probe_spec(Some(model?), McpDeclaration::None)?;
    spec.prompt = MICRO_PROMPT.into();
    Some(spec)
}

fn probe_ctx() -> SpawnCtx {
    SpawnCtx {
        agent_id: marion_core::contract::AgentId("marion-doctor".into()),
        agent_type: "doctor-probe".into(),
        depth: 0,
        node_token: None,
        // A path, and required rather than optional for the same reason `bridge` is: §6.1 step 8
        // gives every typed-plane spawn a readiness marker, so a ctx without one is a ctx no real
        // spawn ever builds. Leaving it `None` made claude-code's declaration step report
        // `MissingInput` — the probe's own omission, printed in the column an operator reads as a
        // finding about their installed binary. §8's whole argument for this mode is that a probe
        // says *why*, and "marion forgot to fill in its own spec" is not a fact about `claude`.
        //
        // Nothing touches it: the two steps that use this ctx compile argv and check a declaration
        // route, and the marker is written by a bridge that is never started here.
        ready_file: Some(std::env::temp_dir().join("marion-doctor/mcp-ready")),
        repo: std::env::temp_dir(),
        state_dir: std::env::temp_dir().join("marion-doctor"),
        // A path, not a process: nothing here launches a bridge. `current_exe` is the honest value
        // because it is what a real spawn would use — `mcp` is a subcommand of this binary (§10).
        bridge: std::env::current_exe().unwrap_or_else(|_| "marion-supervisor".into()),
        bridge_args: vec!["mcp".into()],
    }
}

/// The report an operator reads.
pub fn render(rows: &[Row]) -> String {
    let mut s = String::new();
    for r in rows {
        // The whole key on the header line, `role` included. A reader who takes one line out of
        // this report and quotes it must not be able to turn it into "codex cannot resume".
        s.push_str(&format!(
            "{} {} [{}]\n",
            r.report.harness,
            r.report
                .harness_version
                .as_deref()
                .unwrap_or("version undetermined"),
            r.role.as_str()
        ));
        for n in &r.report.notes {
            s.push_str(&format!("  {n}\n"));
        }
        if let Some(ok) = r.report.adapter_check {
            s.push_str(&format!(
                "  adapter check: {}\n",
                if ok { "PASS" } else { "FAIL" }
            ));
        }
        s.push_str(&format!(
            "  elapsed: {} ms\n\n",
            r.report.elapsed.0.as_millis()
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn a_mode_is_required_and_both_of_section_8s_modes_parse() {
        assert_eq!(
            parse_args(&["--capabilities".into()]).unwrap().mode,
            ProbeMode::Capabilities
        );
        assert_eq!(
            parse_args(&["--adapter".into()]).unwrap().mode,
            ProbeMode::Adapter
        );
        // Neither is the default: the two differ by orders of magnitude in cost.
        assert!(parse_args(&[]).unwrap_err().contains("needs a mode"));
    }

    #[test]
    fn the_harness_filter_is_parsed_and_an_unknown_one_is_refused_by_name() {
        let o = parse_args(&["--adapter".into(), "--harness".into(), "codex".into()]).unwrap();
        assert_eq!(o.harness, Some(Harness::Codex));
        let e = parse_args(&["--adapter".into(), "--harness".into(), "claude_code".into()])
            .unwrap_err();
        assert!(
            e.contains("claude-code"),
            "the refusal lists the spellings: {e}"
        );
        assert!(
            parse_args(&["--adapter".into(), "--harness".into()])
                .unwrap_err()
                .contains("needs a harness name")
        );
    }

    /// A doctor that silently ignored an unknown flag of its own would be in no position to report
    /// unknown configuration keys as load errors, which is the job §3.1 gives it.
    #[test]
    fn an_unknown_flag_is_an_error_rather_than_an_ignore() {
        let e = parse_args(&["--adapter".into(), "--caps".into()]).unwrap_err();
        assert!(e.contains("--caps"), "the refusal names the flag: {e}");
    }

    #[test]
    fn a_model_is_carried_and_never_invented() {
        assert_eq!(parse_args(&["--adapter".into()]).unwrap().model, None);
        assert_eq!(
            parse_args(&["--adapter".into(), "--model".into(), "gpt-5".into()])
                .unwrap()
                .model,
            Some("gpt-5".into())
        );
    }

    #[test]
    fn a_version_token_is_found_in_each_shape_a_harness_prints() {
        assert_eq!(version_token("0.147.0\n").as_deref(), Some("0.147.0"));
        assert_eq!(
            version_token("codex-cli 0.147.0\n").as_deref(),
            Some("0.147.0")
        );
        assert_eq!(
            version_token("2.1.223 (Claude Code)\n").as_deref(),
            Some("2.1.223")
        );
        assert_eq!(version_token("v1.17.3").as_deref(), Some("1.17.3"));
        // And nothing that is not one, because `advertised` compares it numerically and a token it
        // cannot parse must never reach it as though it could.
        for none in ["", "unknown", "0.146", "1.2.3.4", "abc.def.ghi", "1.2.x"] {
            assert_eq!(version_token(none), None, "{none:?}");
        }
    }

    fn caps_rows(harness: Option<Harness>) -> Vec<Row> {
        run(&Options {
            mode: ProbeMode::Capabilities,
            harness,
            model: None,
        })
    }

    /// §8's filter *is* performed — it is the difference between spawning one real agent and four.
    #[test]
    fn the_harness_filter_narrows_what_is_probed() {
        let one = caps_rows(Some(Harness::Codex));
        assert!(one.iter().all(|r| r.report.harness == Harness::Codex));
        let all = caps_rows(None);
        let probed: BTreeSet<Harness> = all.iter().map(|r| r.report.harness).collect();
        assert_eq!(probed.len(), Harness::ALL.len(), "every harness is probed");
        assert!(
            all.len() > one.len(),
            "filtering to one harness must probe strictly fewer keys than probing all four"
        );
    }

    /// **§3.3's key is three components, and this is the assertion that it is not two.**
    ///
    /// claude-code advertises `interrupt` and `permissions`. On its node surface —
    /// `Typed(StreamJson)` — both publish. On its pane surface — §3.4's `opaque`, which cannot
    /// address a turn — `permissions` is clipped and `interrupt` survives. So the two rows differ
    /// in exactly one component of the key and must not collapse into one another: a `run` that
    /// emitted one row per harness, or that keyed capabilities on `(harness, version)` and
    /// attached surfaces as a label, fails here.
    #[test]
    fn two_rows_differing_only_in_surface_do_not_collapse() {
        let rows = caps_rows(Some(Harness::ClaudeCode));
        let node = rows
            .iter()
            .find(|r| r.role == SurfaceRole::Node)
            .expect("a node row");
        let pane = rows
            .iter()
            .find(|r| r.role == SurfaceRole::Pane)
            .expect("claude-code declares a pane surface, so it has a second key");

        assert_eq!(node.report.harness, pane.report.harness, "same harness");
        assert_eq!(
            node.report.harness_version, pane.report.harness_version,
            "same version — the surface is the only component of the key that differs"
        );
        assert_ne!(node.surfaces, pane.surfaces, "and it does differ");
        assert_ne!(
            node.capabilities, pane.capabilities,
            "two keys that differ must be allowed to answer differently; collapsing them is the \
             sentence §9's M1 forbids"
        );

        // Named, so that a change which flips *which* capability differs cannot pass by still
        // being merely unequal.
        assert!(node.capabilities.permissions, "S9 measured the round-trip");
        assert!(
            !pane.capabilities.permissions,
            "a pane cannot address a turn, so a permission request has no channel back to marion"
        );
        assert!(
            node.capabilities.interrupt && pane.capabilities.interrupt,
            "S11 measured interrupt over a real pty; the clip must be specific, not a blanket"
        );

        // And the operator reads the difference rather than inferring it.
        let out = render(&rows);
        assert!(out.contains("[node]") && out.contains("[pane]"), "{out}");
    }

    /// The pane row is the *same binary at a different key*, not a second contract test — so it
    /// must not inherit a verdict for an invocation it never named.
    #[test]
    fn a_pane_row_does_not_inherit_the_node_rows_adapter_verdict() {
        let rows = run(&Options {
            mode: ProbeMode::Adapter,
            harness: Some(Harness::ClaudeCode),
            model: None,
        });
        let pane = rows
            .iter()
            .find(|r| r.role == SurfaceRole::Pane)
            .expect("a pane row");
        assert!(
            pane.report.adapter_check.is_none(),
            "`compile_pane` produces different argv; a verdict copied onto this row would report \
             a check against a launch it never saw"
        );
        assert!(
            pane.report
                .notes
                .iter()
                .any(|n| n.contains("adapter check: NOT RUN on this surface")),
            "and it says so, rather than being silently absent"
        );
    }

    /// §3.3's ceiling rule, asserted on what `doctor` actually publishes rather than on
    /// `static_caps` in isolation. A row that claimed a capability its surface cannot carry would
    /// grey in an action that then fails — §3.3's *degrade visibly* run backwards.
    #[test]
    fn no_row_publishes_a_capability_above_its_surfaces_ceiling() {
        for r in caps_rows(None) {
            let ceiling = Capabilities::ceiling(&r.surfaces);
            assert!(
                r.capabilities.is_at_or_below(&ceiling),
                "{} [{}] published {:?}, above its surface ceiling {:?}",
                r.report.harness,
                r.role.as_str(),
                r.capabilities.granted(),
                ceiling.granted()
            );
        }

        // The general assertion above passes vacuously if nothing ever over-claims, so pin the one
        // row where `advertised` genuinely sits above the ceiling and the meet is what clips it.
        // §9's M1: `codex exec resume` exists on 0.146.0, and `codex exec --json` is `LaunchOnly`.
        let v = "0.147.0";
        assert!(
            marion_harness::advertised(Harness::Codex, v).resume,
            "the software can; this is the half that must stay true for the clip to mean anything"
        );
        let node = caps_rows(Some(Harness::Codex))
            .into_iter()
            .find(|r| r.role == SurfaceRole::Node)
            .expect("a node row");
        assert!(
            !node.capabilities.resume,
            "and the surface is what says it cannot — doctor publishes \"codex on this surface \
             cannot\", never \"codex cannot\""
        );
    }

    /// A version marion could not read must be *reported as undetermined*, never omitted and never
    /// guessed. `advertised` compares numerically, so a guess here would publish a capability
    /// marion has not measured — and an omission would let a reader supply their own guess.
    #[test]
    fn an_undetermined_version_is_reported_as_such_and_never_stands_in_for_a_measurement() {
        // `advertised`'s side: nothing marion cannot parse may clear a measured floor.
        for unreadable in ["", "unknown", "0.146", "latest"] {
            assert!(
                !marion_harness::advertised(Harness::Codex, unreadable).resume,
                "{unreadable:?} is not a version marion has measured"
            );
        }

        // The key's side, on the surface where a clip would otherwise hide the guess. `codex exec
        // --json` is `LaunchOnly`, so `resume` is false there whatever the version says; an
        // app-server surface is where the version alone decides, and it is where a fallback that
        // keyed "unread" as latest would publish a capability nobody measured.
        let addressable = ExecutionSurfaces::headless(marion_harness::TypedKind::AppServer);
        assert!(
            static_caps(Harness::Codex, "0.147.0", &addressable).resume,
            "the surface carries it and the software has it — so this row can tell the two \
             fallback directions apart"
        );
        assert!(
            !static_caps(Harness::Codex, keyed_version(None), &addressable).resume,
            "an unread version must key below every measured floor, never at or above one"
        );

        // And the report's side: the row prints the words rather than an empty column.
        let mut row = caps_rows(Some(Harness::Codex)).remove(0);
        row.report.harness_version = None;
        let out = render(&[row]);
        assert!(
            out.contains("version undetermined"),
            "an unread version must be named in the header, not left blank: {out}"
        );
        assert!(
            !out.contains("codex  ["),
            "and not rendered as an empty slot a reader fills in themselves: {out}"
        );
    }

    /// The load-bearing half of the version-without-a-model answer: gemini and opencode make an
    /// explicit model a MUST at `compile`, and marion may not choose one (§6.4). The placeholder
    /// exists so those two can still be *named and version-read*, and it is only honest as long as
    /// no process is ever started from a spec carrying it.
    #[test]
    fn the_placeholder_model_never_reaches_a_spec_anything_is_launched_from() {
        // The one spec a process is started from refuses to exist without a real model...
        assert!(
            probe_spec_with_prompt(None).is_none(),
            "no model, no live turn — a version probe may not be paid for with an invented model"
        );
        // ...and when it does exist it carries the operator's model, not the placeholder.
        let spec = probe_spec_with_prompt(Some("gpt-5".into())).expect("a real model was given");
        assert_eq!(spec.model.as_deref(), Some("gpt-5"));
        assert_ne!(spec.model.as_deref(), Some(PLACEHOLDER_MODEL));
        assert_eq!(spec.prompt, MICRO_PROMPT, "and it is the prompt that runs");

        // The placeholder is confined to the specs that launch nothing, and it is not mistakable
        // for a model id any provider would route.
        assert_eq!(
            probe_spec(None, McpDeclaration::None)
                .unwrap()
                .model
                .as_deref(),
            Some(PLACEHOLDER_MODEL)
        );
        assert!(
            PLACEHOLDER_MODEL.starts_with("marion-doctor/"),
            "it must read as marion's own placeholder, never as a vendor model"
        );
    }

    /// The whole point of the placeholder, measured end to end: the two adapters that require a
    /// model still report a version, and they report a *read* one rather than a claimed one.
    #[test]
    fn a_harness_that_requires_a_model_still_reports_a_version_without_one() {
        for h in [Harness::Gemini, Harness::OpenCode] {
            let rows = caps_rows(Some(h));
            let notes = rows[0].report.notes.join("\n");
            assert!(
                notes.contains("binary:"),
                "{h}: the program name must be readable off a compiled argv without a model:\n\
                 {notes}"
            );
            // Either it was read, or it says why not. What is forbidden is silence.
            let named = rows[0].report.harness_version.is_some()
                || notes.contains("version: FAILED")
                || notes.contains("not found on $PATH");
            assert!(named, "{h}: neither a version nor a reason:\n{notes}");
            if let Some(v) = &rows[0].report.harness_version {
                assert_eq!(
                    version_token(v).as_deref(),
                    Some(v.as_str()),
                    "{h}: a published version must be one `advertised` can parse"
                );
            }
        }
    }

    /// §9's M1 sentence, in the thing an operator actually reads. Every row names its surfaces, and
    /// no row says "cannot resume" without saying on what.
    #[test]
    fn every_row_names_the_surfaces_its_capability_answer_is_keyed_on() {
        let rows = caps_rows(None);
        let out = render(&rows);
        for r in &rows {
            assert_eq!(
                r.capabilities,
                static_caps(
                    r.report.harness,
                    r.report.harness_version.as_deref().unwrap_or(""),
                    &r.surfaces
                ),
                "doctor must publish `static_caps` on its own key, not a parallel table"
            );
            assert!(
                r.report
                    .notes
                    .iter()
                    .any(|n| n.starts_with(&format!("surfaces ({}):", r.role.as_str()))),
                "{} [{}] published a capability answer without naming the surfaces it is keyed on",
                r.report.harness,
                r.role.as_str()
            );
            assert!(
                r.report.adapter_check.is_none(),
                "--capabilities runs no micro-contract, and \"did not run\" must not decay to false"
            );
        }
        for h in Harness::ALL {
            assert!(out.contains(h.as_str()), "{h} is missing from the report");
        }
    }

    /// **A probe's own omissions must never be printed in the column that reports the operator's
    /// binary.**
    ///
    /// This caught a real one. `probe_ctx` left `ready_file` as `None`, so claude-code's
    /// `compile` — which requires the §6.1 step 8 readiness marker every real spawn supplies —
    /// returned `MissingInput`, and `--adapter` printed `declaration: FAILED` and exited non-zero
    /// against a perfectly good `claude`. §8's argument for this mode is that a probe says *why* a
    /// harness is unusable; "marion did not fill in its own spec" is not a fact about the harness,
    /// and as a CI gate it is a false red.
    #[test]
    fn the_probes_own_spec_is_complete_enough_for_every_adapter_to_compile() {
        let ctx = probe_ctx();
        let spec = probe_spec(None, McpDeclaration::Marion).expect("a spec");
        for h in Harness::ALL {
            let a = adapter_for(h).unwrap();
            let inv = a.compile(&spec, &ctx).unwrap_or_else(|e| {
                panic!(
                    "{h}: the probe handed the adapter a spec no real spawn would build — this \
                     would print as a finding about the installed binary: {e}"
                )
            });
            let files = a
                .config_files(&spec, &ctx)
                .unwrap_or_else(|e| panic!("{h}: {e}"));
            let written: Vec<PathBuf> = files.into_iter().map(|(p, _)| p).collect();
            assert!(
                a.mcp_route(&spec).verify(&written, &inv).is_ok(),
                "{h}: the declaration route the adapter promises must be taken by what the probe \
                 compiles, or the probe reports its own gap as a missing bridge"
            );
        }
    }

    /// `--adapter` is §8's CI gate, so a PASS must mean the binary passed and not that no step
    /// ran. Every harness installed on this machine reaches a verdict, and no verdict is reached
    /// by way of a step that failed for want of something marion should have supplied.
    #[test]
    fn no_adapter_verdict_is_decided_by_the_probes_own_missing_input() {
        for r in run(&Options {
            mode: ProbeMode::Adapter,
            harness: None,
            model: None,
        }) {
            let notes = r.report.notes.join("\n");
            assert!(
                !notes.contains("declaration: FAILED — compile:"),
                "{} [{}] failed to compile the probe's spec; that is marion's gap, not the \
                 binary's:\n{notes}",
                r.report.harness,
                r.role.as_str()
            );
        }
    }

    /// The two names §8 requires and this probe cannot honour must appear as refusals, on every
    /// `--adapter` report, in the same place a failure would appear.
    #[test]
    fn the_checks_this_probe_cannot_run_are_named_rather_than_omitted() {
        let rows = run(&Options {
            mode: ProbeMode::Adapter,
            harness: Some(Harness::ClaudeCode),
            model: None,
        });
        let notes = rows[0].report.notes.join("\n");
        assert!(
            notes.contains("keystroke-injection submit: NOT RUN"),
            "§8 requires it; a probe that omits it reads as having passed it:\n{notes}"
        );
        assert!(
            notes.contains("live turn: NOT RUN"),
            "claude-code's typed plane writes its prompt after launch; that is `duplex`'s \
             protocol, not a probe's:\n{notes}"
        );
        assert!(
            rows[0].report.adapter_check.is_some(),
            "--adapter always reaches a verdict, even when steps were skipped"
        );
    }
}
