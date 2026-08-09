# Native Facade Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Establish the vendor-neutral registry, exact OS-value wire types, CLI seam, and root validation needed for `marion <harness> <native args>` without advertising or launching an incomplete facade.

**Architecture:** Keep command discovery and resolution pure in `marion-core`; keep exact, versioned launch data in `marion-proto`; let the `marion` binary recognize only registry entries before its legacy parser; and carry a root-only native context through the supervisor while an explicit readiness gate still refuses launch. This slice deliberately stops before adapter injection and transparent PTY proxying, so no user-visible facade can claim compatibility prematurely.

**Tech Stack:** Rust 1.94, edition 2024, serde/JSON-RPC, Unix `OsStrExt`/`OsStringExt`, existing Cargo workspace tests, `.githooks/pre-commit`, and Sentrux.

## Global Constraints

- Preserve user program, argv element boundaries, argv order, cwd, and environment values as opaque OS values. On Unix, invalid UTF-8 bytes must round-trip exactly; conversion through `String` is forbidden.
- Resolve facade commands only from a validated registry. Never fall through to an arbitrary executable found on `PATH`.
- Reserve Marion's existing control-plane vocabulary (`run`, `attach`, `tree`, `mcp`, and top-level help/version spellings). A descriptor or alias collision is a startup/test failure, never precedence by registration order.
- Native launch context is root-only. Child nodes continue through Marion's typed adapter/ACP paths and inherit the root's resolved launch policy; a child may not assert a new native context.
- Unknown launch-context versions and new-client/old-supervisor combinations fail loudly. They must never downgrade to managed/headless execution.
- Do not advertise a facade until its adapter injection and transparent foreground PTY transport are both ready. The production registry remains empty in this slice.
- Keep generic `marion attach` grid behavior unchanged. Transparent byte proxying belongs to a later facade-specific transport slice.
- Put harness-specific injection behind adapter-owned behavior in the later slice. This slice adds no `match "claude"`/`match "codex"` branches.
- Preserve existing public paths for moved behavior-neutral items: both `marion_harness::Auth` and `marion_harness::adapter::Auth`, and `marion_supervisor::root::ROOT_DEPTH`.
- Preserve the dependency direction in the main design: `marion-core` stays pure/no I/O and does not depend on protocol, supervisor, terminal, or harness crates.
- Tests may use synthetic facade descriptors. Production must have zero ready descriptors and every new critical assertion needs a named mutation check.
- No test may silently skip because a binary, credential, platform feature, or fixture is absent. This slice's exact-byte evidence is Unix/macOS-only and must say so explicitly.
- Sentrux quality must not regress from the recorded baseline **5146**; acyclicity is the current bottleneck. Run a separate cleanup review after the feature tasks.
- Preserve unrelated operator work, especially `docs/research/2026-08-09-zed-acp-antigravity.md` and gitignored task notes.

---

## Task 1: Break the two measured dependency backedges without changing behavior

**Files:**

- Create: `crates/marion-harness/src/auth.rs`
- Modify: `crates/marion-harness/src/lib.rs`
- Modify: `crates/marion-harness/src/adapter.rs`
- Modify: `crates/marion-harness/src/{claude_code,codex,gemini,opencode}.rs`
- Create: `crates/marion-harness/tests/auth_public_paths.rs`
- Create: `crates/marion-supervisor/src/depth.rs`
- Modify: `crates/marion-supervisor/src/lib.rs`
- Modify: `crates/marion-supervisor/src/{root,bridge,run,duplex,events}.rs`
- Test: `crates/marion-supervisor/tests/{auth_mode,report_on_a_root}.rs`

- [ ] **1.1 Write compile-level compatibility tests first.** In `auth_public_paths.rs`, type-check both public imports and assert their `as_wire`/`from_wire` behavior is identical. Preserve the existing external `root::ROOT_DEPTH` import in `report_on_a_root.rs`; add an explicit `assert_eq!(ROOT_DEPTH, 0)` only if the test does not already make the value observable.

Run:

```bash
CARGO_NET_OFFLINE=true cargo test -p marion-harness --test auth_public_paths --locked --offline
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor --test auth_mode --test report_on_a_root --locked --offline
```

Expected baseline green: `marion_harness::Auth` is already re-exported from `adapter`; this test pins that public contract before ownership moves. The red evidence for this task is the explicit compatibility-re-export mutation in step 1.5.

- [ ] **1.2 Move `Auth` into a leaf.** `auth.rs` owns the enum and its unchanged manual wire methods. It imports nothing from other Marion modules. Use:

```rust
// lib.rs
pub(crate) mod auth;
pub use auth::Auth;

// adapter.rs — compatibility path
pub use crate::auth::Auth;
```

Redirect the four harness helpers to `crate::auth::Auth`; do not change enum variants, debug text, or `MARION_AUTH` spellings.

- [ ] **1.3 Move `ROOT_DEPTH` into a leaf.** `depth.rs` contains only `pub const ROOT_DEPTH: u32 = 0;`. Keep the leaf module crate-private, re-export from `root.rs` with `pub use crate::depth::ROOT_DEPTH;`, and redirect internal consumers/tests to `crate::depth::ROOT_DEPTH` unless they intentionally verify the public compatibility path.

- [ ] **1.4 Prove behavior and graph intent.** Run the two commands above plus:

```bash
CARGO_NET_OFFLINE=true cargo test -p marion-harness --lib --locked --offline
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor --lib --locked --offline
rg -n 'adapter::Auth|root::ROOT_DEPTH' crates/marion-harness/src crates/marion-supervisor/src
```

Expected green: only intentional compatibility re-exports/public-path tests remain; leaf modules have no Marion-module imports.

- [ ] **1.5 Mutation check.** Temporarily remove each compatibility re-export in turn and require its named external-path test to fail to compile, then restore and rerun green.

- [ ] **1.6 Commit:** `refactor: isolate auth and root depth leaves`

---

## Task 2: Add a pure, validated native-facade registry

**Files:**

- Create: `crates/marion-core/src/native_facade.rs`
- Modify: `crates/marion-core/src/lib.rs`
- Test: unit tests in `crates/marion-core/src/native_facade.rs`

- [ ] **2.1 Write resolver tests first.** Cover exact primary-name lookup, explicit alias lookup, case sensitivity, unknown input, reserved primary names, reserved aliases, duplicate primaries, alias-to-primary collisions, alias-to-alias collisions, and the fact that an executable present on `PATH` is still unknown. Use synthetic descriptors only.

The public vocabulary should be structurally equivalent to:

```rust
pub struct NativeFacadeDescriptor {
    pub command: &'static str,
    pub aliases: &'static [&'static str],
    pub agent_type: &'static str,
    pub readiness: NativeFacadeReadiness,
}

pub enum NativeFacadeReadiness {
    Planned,
    Ready,
}

pub struct NativeFacadeRegistry<'a> { /* validated borrowed descriptors */ }
```

The constructor returns a typed validation error; resolution returns only a descriptor from the registry, never a program path. Planned descriptors are discoverable to internal diagnostics but absent from `ready_commands()` and cannot resolve for launch.

Run:

```bash
CARGO_NET_OFFLINE=true cargo test -p marion-core native_facade --locked --offline
```

Expected red: module/types do not exist.

- [ ] **2.2 Implement one validation pass and one exact resolver.** Centralize the reserved set in this module. Reject empty/non-ASCII command tokens, leading `-`, collisions, and reserved spellings. Do not inspect the filesystem or environment.

- [ ] **2.3 Pin the production readiness gate.** Export an empty production descriptor slice (or an equivalent constructor used by the CLI) and test:

```rust
assert!(production_native_facades().ready_commands().is_empty());
```

No `claude`, `codex`, `gemini`, `agy`, or `opencode` literal belongs in this production slice yet.

- [ ] **2.4 Mutation checks.** Temporarily (a) allow a reserved alias, (b) use last-registration-wins for a collision, and (c) return `Ready` for a planned descriptor. Require the corresponding unit test to fail for the intended reason, restore, and rerun green.

- [ ] **2.5 Commit:** `feat: add validated native facade registry`

---

## Task 3: Define versioned exact-OS-value launch context on the wire

**Files:**

- Modify: root `Cargo.toml` workspace dependencies if needed
- Modify: `crates/marion-proto/Cargo.toml`
- Create: `crates/marion-proto/src/native.rs`
- Modify: `crates/marion-proto/src/lib.rs`
- Modify: `crates/marion-proto/src/params.rs`
- Test: unit tests in `crates/marion-proto/src/native.rs` and existing params round trips

- [ ] **3.1 Write exact-byte and schema tests first.** On Unix, construct program/argv/cwd/env keys and values containing non-UTF-8 bytes with `OsStringExt::from_vec`; serialize to JSON, deserialize, and assert byte-for-byte equality. Also cover empty args, empty env values, argument boundaries, geometry, malformed base64, `wire_version != 1`, and unknown fields.

Use a stable representation equivalent to:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpaqueOsValueV1(Vec<u8>); // custom base64 serde, no String conversion

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEnvVarV1 {
    pub name: OpaqueOsValueV1,
    pub value: OpaqueOsValueV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalGeometryV1 {
    pub cols: u16,
    pub rows: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeLaunchContextV1 {
    pub wire_version: u16, // exactly 1
    pub program: OpaqueOsValueV1,
    pub argv: Vec<OpaqueOsValueV1>,
    pub cwd: OpaqueOsValueV1,
    pub env: Vec<NativeEnvVarV1>,
    pub geometry: TerminalGeometryV1,
}
```

Implement `Deserialize` through a private `#[serde(deny_unknown_fields)]` wire struct followed by `TryFrom`, and refuse any `wire_version` other than `1` before constructing `NativeLaunchContextV1`. If `base64` is added directly, pin the already-locked compatible version and use one documented alphabet/padding rule. Decode errors must name the field/value class without echoing arbitrary environment contents.

Run:

```bash
CARGO_NET_OFFLINE=true cargo test -p marion-proto native --locked --offline
```

Expected red: the types and `AgentSpawnParams::native_launch` field do not exist.

- [ ] **3.2 Add the optional additive field.** Add `#[serde(default, skip_serializing_if = "Option::is_none")] pub native_launch: Option<NativeLaunchContextV1>` to `AgentSpawnParams`. Update every literal constructor explicitly rather than hiding the migration behind `Default`. Amend the existing serialization comment that says every field is written to name this backward-compatible exception. Add an exact old-frame deserialize/reserialize assertion proving bytes/JSON stay unchanged when the field is absent; a frame carrying an unknown field/version must be refused loudly.

- [ ] **3.3 Add platform conversion APIs.** Under `cfg(unix)`, provide exact `OsStr`/`OsString` conversions. Under non-Unix targets, expose a typed unsupported-platform error rather than a lossy fallback.

- [ ] **3.4 Run all protocol tests.**

```bash
CARGO_NET_OFFLINE=true cargo test -p marion-proto --locked --offline
```

- [ ] **3.5 Mutation checks.** Replace one Unix conversion with `to_string_lossy()` and require the invalid-UTF-8 test to fail. Accept `wire_version = 2` and require the version-refusal test to fail. Restore both and rerun green.

- [ ] **3.6 Commit:** `feat: add opaque native launch protocol`

---

## Task 4: Add an `args_os` first-token facade seam without changing legacy behavior

**Files:**

- Create: `crates/marion-supervisor/src/facade_cli.rs`
- Modify: `crates/marion-supervisor/src/lib.rs`
- Modify: `crates/marion-supervisor/src/bin/marion.rs`
- Test: unit tests in `crates/marion-supervisor/src/facade_cli.rs`
- Test: existing CLI integration targets (`attach_verb`, `client_run`, `launch_only_root`, `mcp_conformance`)

- [ ] **4.1 Write pure routing tests first.** With a synthetic ready registry, prove that only argv[1] is interpreted as an ASCII facade selector and the remaining `OsString`s are returned byte-for-byte, including `--help`, `--version`, `--`, positionals, empty strings, and invalid UTF-8. Prove reserved/unknown/planned selectors return control to the existing Marion parser.

The seam should be equivalent to:

```rust
pub struct NativeFacadeInvocation<'a> {
    pub descriptor: &'a NativeFacadeDescriptor,
    pub argv: Vec<OsString>,
}

pub fn resolve_native_invocation<'a>(
    argv: impl IntoIterator<Item = OsString>,
    registry: &'a NativeFacadeRegistry<'a>,
) -> Result<Option<NativeFacadeInvocation<'a>>, FacadeCliError>;
```

Run:

```bash
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor facade_cli --locked --offline
```

Expected red: the module does not exist.

- [ ] **4.2 Integrate ahead of, not inside, the legacy `String` parser.** Use `std::env::args_os()` only for the facade probe. If it returns `None`, separately call the existing `std::env::args().skip(1).collect::<Vec<String>>()` path unchanged, including its current all-argument `-h`/`--help` scan, attach/tree/mcp/run dispatch, usage text, and exit codes. Never convert unmatched `OsString`s through a new lossy/error path, and never round-trip a matched facade tail through `String`.

- [ ] **4.3 Pin zero public behavior in this slice.** A CLI test must prove the production registry exposes no ready command and that `marion claude`, `marion codex`, and an arbitrary executable name still take the existing usage/refusal path. This test is deleted or changed only in the later adapter+PTY enablement slice.

- [ ] **4.4 Run legacy binary/process tests.**

```bash
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor --test attach_verb --locked --offline
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor --test client_run --locked --offline
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor --test launch_only_root --locked --offline
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor --test mcp_conformance --locked --offline
```

- [ ] **4.5 Mutation checks.** Convert the synthetic facade tail with `into_string()` and require the invalid-byte test to fail. Make the production registry contain one ready descriptor and require the zero-public-behavior test to fail. Restore and rerun green.

- [ ] **4.6 Commit:** `feat: add opaque facade cli routing seam`

---

## Task 5: Carry and validate native context at the root boundary, then refuse launch

**Files:**

- Modify: `crates/marion-supervisor/src/handler.rs`
- Modify: `crates/marion-supervisor/src/root.rs`
- Modify: `crates/marion-supervisor/src/bin/marion.rs` constructors only as required
- Test: handler unit tests near existing `root_params`
- Test: `crates/marion-supervisor/tests/client_run.rs` or a new focused `native_facade_gate.rs`

- [ ] **5.1 Write refusal/side-effect tests first.** Cover:
  - a child request (`caller: Some`) carrying `native_launch` is refused by name;
  - version other than 1 is refused before journal/process/PTY side effects;
  - a valid root context reaches `RootSpec` unchanged in a pure construction test;
  - launch still refuses `native facade transport is not ready` before process creation;
  - legacy root requests with `native_launch: None` behave exactly as before.

Run the narrow test first:

```bash
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor native_launch --locked --offline
```

Expected red: handler/root do not know the field.

- [ ] **5.2 Add one root-boundary validator and one pure builder seam.** Keep pairing/version/platform/readiness decisions in one pure function called before spawn intent, journal mutation, registry claim, PTY allocation, or process launch. Its error variants must distinguish child misuse, unsupported version/platform, and transport-not-ready. Extract the existing embedded `RootSpec` assembly from `handler::spawn_root` into a named pure helper such as `root_spec_from_spawn`; use it in production and unit-test that it preserves a valid native context unchanged.

- [ ] **5.3 Extend `RootSpec` without compiling it.** Add `native_launch: Option<NativeLaunchContextV1>` and thread the field from `AgentSpawnParams`. Do not call a harness adapter, synthesize argv, or modify `compile_pane` in this slice.

- [ ] **5.4 Prove no silent downgrade.** The process-boundary test submits a valid V1 context over the real socket and requires a non-zero/refusal result naming readiness, zero node/process creation, and no managed/headless invocation artifact.

- [ ] **5.5 Mutation checks.** Remove the caller pairing check and require the child-misuse test to fail. Change readiness refusal into `native_launch = None` and require the no-silent-downgrade/process-boundary test to fail. Restore and rerun green.

- [ ] **5.6 Commit:** `feat: validate root native launch context`

---

## Task 6: Review, mutation evidence, quality gate, and handoff to the adapter slice

**Files:**

- Modify: this plan's ignored SDD ledger only
- Do not add another committed status document

- [ ] **6.1 Run a fresh cleanup subagent.** It reviews the Task 1 graph changes and Tasks 2–5 boundaries for duplicated resolution, harness-name branching, public API drift, lossy conversions, and readiness bypasses. Fix all Important/Critical findings through the task review loop.

- [ ] **6.2 Record mutation evidence.** For every mutation named above, record the exact patch, failing test, assertion/failure reason, restore, and green rerun in the SDD task reports. A compile failure counts only for public-path compatibility mutations.

- [ ] **6.3 Run formatting, lint, and focused suites.**

```bash
cargo fmt --all -- --check
CARGO_NET_OFFLINE=true cargo clippy --workspace --all-targets --locked --offline -- -D warnings
CARGO_NET_OFFLINE=true cargo test -p marion-core --locked --offline
CARGO_NET_OFFLINE=true cargo test -p marion-proto --locked --offline
CARGO_NET_OFFLINE=true cargo test -p marion-harness --locked --offline
CARGO_NET_OFFLINE=true cargo test -p marion-supervisor --lib --locked --offline
CARGO_NET_OFFLINE=true sh .githooks/pre-commit
```

- [ ] **6.4 Run the entire workspace outside the sandbox when local socket tests require it.**

```bash
CARGO_NET_OFFLINE=true cargo test --workspace --all-targets --locked --offline
```

Expected: zero failures and zero silent skips. Compare with the pre-change baseline, including the real provider socket and pane-attach suites.

- [ ] **6.5 Close the Sentrux session.** Run the `sentrux:scan` session-end workflow. Require overall quality `>= 5146`, no new dependency cycle, and a non-regressed acyclicity score. If a score falls, assign a cleanup worker before proceeding.

- [ ] **6.6 Final whole-slice review.** Use a fresh high-capability reviewer with the plan brief, task reports, full diff package, mutation ledger, and Sentrux result. Resolve findings through one scoped fix wave and re-review.

- [ ] **6.7 Update `tasks/todo.md` Review locally.** Record exact commands/results and link the next plan. Do not commit the gitignored task ledger.

- [ ] **6.8 Commit any review-only fixes** in the smallest logical commit; do not squash the task commits.

---

## Out of Scope for This Plan

- Enabling any production `marion <harness>` selector.
- Claude/Codex/OpenCode/Gemini/Antigravity-specific MCP/config injection.
- Transparent PTY proxying, atomic replay/live splice, byte-exact PTY notifications, terminal restoration, signal/exit propagation, or resize/mouse behavior.
- ACP manifest installation or Zed-style dynamic registry distribution.
- The 4×5 deterministic adapter matrix, pinned live TUI boots, or Marion-on-Marion dogfood.

Those belong to the next three plans in this order: adapter-owned native injection; transparent PTY and lifecycle; deterministic/live compatibility matrix.

## Plan Self-Review

- [ ] Every task is independently red/green and names an exact test command.
- [ ] No task enables or advertises an incomplete facade.
- [ ] No type crosses crate dependency direction.
- [ ] Every opaque value stays byte-exact on Unix.
- [ ] Legacy command/process behavior has explicit regression coverage.
- [ ] Every critical assertion has a named mutation.
- [ ] Cleanup and Sentrux gates recur before the next slice.
- [ ] A placeholder-marker scan is empty.
