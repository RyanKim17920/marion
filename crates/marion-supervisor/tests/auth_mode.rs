//! **What `--base-url` does under real auth, on each of the four harnesses.**
//!
//! Every assertion here is a *measurement of what marion does today*. Four of them pin an adapter
//! behaviour that is still wrong in itself, labelled `CURRENT BEHAVIOUR, NOT DESIRED` at the
//! assertion with the message stating what the right answer would look like — so a fix has a
//! starting point, and so this file fails loudly when one lands rather than encoding the drop as
//! correct and quietly outliving it.
//!
//! # The defect, and what was done about it
//!
//! `marion run <type> --base-url <url>` under real auth (`Auth::Inherited`, the default since
//! `--canned` inverted the flag) used to be **accepted, promised, and silently dropped**: the CLI
//! took the endpoint, the usage text advertised it, `root::compile` copied it into
//! `LaunchSpec.base_url`, and then every adapter threw it away. An operator pointed marion at a
//! corporate gateway, marion said nothing, and the node reached the vendor directly with a real
//! credential — traffic leaving the perimeter the gateway existed to hold, logged nowhere they
//! could see.
//!
//! **The CLI now refuses it.** `bin/marion.rs`'s `resolve_base_url` returns an error naming the
//! flag and saying it is not implemented under real auth, before anything launches; that refusal
//! and the corrected usage text are pinned by that binary's own unit tests. Refusing rather than
//! honouring is the reversible direction — the `background` and `verification` precedent in
//! `spawn::SpawnError` — and honouring a gateway can land later against a stated claim.
//!
//! # Why the four adapter pins stay
//!
//! They now describe a state **unreachable through the CLI**, and that is exactly why they are
//! worth keeping: the adapters themselves are unchanged, and each still silently drops a base URL
//! if one reaches it by any other route — `run_spawn`'s child path, a future caller of
//! `root::compile`, or a `LaunchSpec` built in a test. The CLI gate is one layer; these are the
//! layer under it. Read them as defence in depth against a recurrence, not as a live operator-facing
//! defect. If honouring the endpoint ever lands, they invert; if a second entry point appears that
//! forgets to gate, they are what catches it.
//!
//! # Zero cost
//!
//! Nothing here launches a binary or opens a socket. `compile` and `config_files` are pure — they
//! return argv, an env block, and `(path, contents)` pairs, and write no file — so the whole file
//! is an inspection of what *would* have been spawned. That is also the only way to observe this
//! defect at all: it is the **absence** of an overlay, and a launched process cannot be asked what
//! it was not told.
//!
//! ```sh
//! cargo test -p marion-supervisor --test auth_mode
//! ```

use std::path::PathBuf;

use marion_core::contract::AgentId;
use marion_harness::{
    Auth, ClaudeCodeAdapter, CodexAdapter, Extras, GeminiAdapter, HarnessAdapter, Invocation,
    LaunchSpec, McpDeclaration, OpenCodeAdapter, SpawnCtx,
};

/// The endpoint an operator would actually pass: **non-loopback and https**, so it survives every
/// gate marion does have. `resolve_base_url` accepts it (a loopback one is refused under real auth,
/// which is a different and working path), and `gemini::base_url_is_acceptable` accepts it (plain
/// http off-loopback is refused, again a different and working path). Nothing between the flag and
/// the adapter objects to this URL — it is simply not used.
const GATEWAY: &str = "https://gateway.corp.example/v1";

/// The needle, without the `/v1` the adapters variously strip: Claude Code's
/// `anthropic_base_url` and gemini's `google_base_url` both drop the suffix, so a search for the
/// full URL could pass by missing a value that *is* there.
const GATEWAY_HOST: &str = "gateway.corp.example";

fn ctx() -> SpawnCtx {
    SpawnCtx {
        agent_id: AgentId("019f-root".into()),
        agent_type: "claude".into(),
        depth: 0,
        ready_file: Some("/state/x/mcp-ready".into()),
        repo: "/repo".into(),
        state_dir: "/state".into(),
        bridge: "/bin/marion-supervisor".into(),
        bridge_args: vec!["mcp".into()],
    }
}

/// A spec carrying the gateway, in the mode the flag is documented to be legitimate in.
///
/// `api_key: None` and `model` set are what `root::compile` really builds under `Inherited`: no
/// credential is placed (the node presents the operator's own login) and every harness but codex
/// needs an explicit model. This is the live spec verbatim apart from `base_url`, so what the tests
/// below observe is the effect of the endpoint and of nothing else.
fn live_spec_with_gateway(model: Option<&str>) -> LaunchSpec {
    LaunchSpec {
        cwd: "/repo".into(),
        model: model.map(str::to_string),
        prompt: String::new(),
        allowed_tools: vec!["mcp__marion__spawn".into()],
        mcp: McpDeclaration::Marion,
        base_url: Some(GATEWAY.into()),
        api_key: None,
        auth: Auth::Inherited,
        config_dir: "/state/x/config".into(),
        extra: Extras::default(),
    }
}

/// The same spec under marion's own provider, as each test's **negative control**: it proves the
/// search below is looking somewhere a base URL can actually land, so "the gateway appears nowhere"
/// cannot pass by meaning "this adapter carries no endpoint in either mode".
fn canned_spec_with_gateway(model: Option<&str>) -> LaunchSpec {
    LaunchSpec {
        auth: Auth::Canned,
        api_key: Some("sk-canned".into()),
        ..live_spec_with_gateway(model)
    }
}

fn env_value<'a>(inv: &'a Invocation, key: &str) -> Option<&'a str> {
    inv.env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Every byte a harness would be configured with: argv, the env block, and the contents of each
/// generated config file. A base URL that reached the node reaches one of these.
fn everything_the_node_is_told(
    adapter: &dyn HarnessAdapter,
    spec: &LaunchSpec,
) -> (Invocation, String) {
    let inv = adapter.compile(spec, &ctx()).expect("the spec compiles");
    let files: Vec<(PathBuf, String)> = adapter
        .config_files(spec, &ctx())
        .expect("the config files derive");
    let mut blob = format!("{:?}\n{:?}\n", inv.args, inv.env);
    for (p, c) in &files {
        blob.push_str(&format!("--- {}\n{c}\n", p.display()));
    }
    (inv, blob)
}

/// `MARION_BASE_URL` in the bridge declaration is **marion's own key, not a provider endpoint**: it
/// tells the child's bridge where a *canned* child would talk, and `main::auth_from_env` discards it
/// outright under `Inherited`. It is not the harness being pointed anywhere, so it is excluded from
/// the searches below — a test that counted it would read "the gateway is honoured" off a value
/// that reaches no vendor.
fn without_the_bridge_declaration(blob: &str) -> String {
    blob.lines()
        .filter(|l| !l.contains(marion_supervisor::root::BASE_URL_ENV))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------------------------
// One test per harness. Each states its own mechanism, because the four differ and a fix to one
// is not a fix to the others.
//
// **Defence in depth, not a live operator-facing defect.** `bin/marion.rs` now refuses
// `--base-url` under real auth before anything launches, so no CLI invocation reaches these code
// paths carrying an endpoint. The adapters are unchanged, though, and each still drops one
// silently — so these pin the layer under the gate, against a second entry point that forgets to
// gate, and against the drop being mistaken for intent if honouring is ever implemented.
// ---------------------------------------------------------------------------------------------

/// **CURRENT BEHAVIOUR, NOT DESIRED.** `ClaudeCodeAdapter::compile` matches on `spec.auth` and the
/// `Auth::Inherited` arm is `(None, None)` — base URL and key discarded together, one line, no
/// branch on whether an endpoint was asked for. `compile_headless` then emits `ANTHROPIC_BASE_URL`
/// only when given a value, so the node resolves `api.anthropic.com` itself and the gateway is
/// never mentioned to it.
#[test]
fn claude_code_drops_a_gateway_base_url_under_real_auth() {
    // Control: under `--canned` the same field lands as `ANTHROPIC_BASE_URL`, minus the `/v1` the
    // CLI appends for itself. So this adapter does have a place to put an endpoint.
    let canned = ClaudeCodeAdapter
        .compile(&canned_spec_with_gateway(Some("haiku")), &ctx())
        .expect("the canned spec compiles");
    assert_eq!(
        env_value(&canned, "ANTHROPIC_BASE_URL"),
        Some("https://gateway.corp.example"),
        "negative control: under --canned this adapter does carry the base URL it was handed"
    );

    let live = ClaudeCodeAdapter
        .compile(&live_spec_with_gateway(Some("haiku")), &ctx())
        .expect("the live spec compiles");
    assert_eq!(
        env_value(&live, "ANTHROPIC_BASE_URL"),
        None,
        "CURRENT BEHAVIOUR, NOT DESIRED: marion was handed {GATEWAY} and told this node nothing \
         about it, so a `claude` holding the operator's real OAuth token resolves \
         api.anthropic.com and leaves the perimeter the gateway exists to hold. The CLI now \
         refuses this before it gets here, so this is the layer under that gate rather than a \
         reachable defect — but the drop is still silent, and the right answer if it is ever \
         honoured is to emit ANTHROPIC_BASE_URL from the endpoint under Inherited too: live mode \
         withholds a *credential* marion minted, which a gateway URL is not. When that lands, \
         invert this assertion. Compiled env: {:?}",
        live.env
    );
}

/// **CURRENT BEHAVIOUR, NOT DESIRED.** `gemini::compile_prompt` pushes `GOOGLE_GEMINI_BASE_URL`
/// inside `if spec.auth == Auth::Canned`, so the endpoint is gated on the **mode** and not only on
/// the value. The adapter above it also validates the URL first
/// (`gemini::base_url_is_acceptable`) — so a live run can be *refused* for a malformed gateway it
/// would then have ignored anyway.
#[test]
fn gemini_drops_a_gateway_base_url_under_real_auth() {
    let canned = GeminiAdapter
        .compile(&canned_spec_with_gateway(Some("gemini-2.5-flash")), &ctx())
        .expect("the canned spec compiles");
    assert_eq!(
        env_value(&canned, "GOOGLE_GEMINI_BASE_URL"),
        Some("https://gateway.corp.example"),
        "negative control: under --canned this adapter does carry the base URL it was handed"
    );

    let live = GeminiAdapter
        .compile(&live_spec_with_gateway(Some("gemini-2.5-flash")), &ctx())
        .expect("the live spec compiles");
    assert_eq!(
        env_value(&live, "GOOGLE_GEMINI_BASE_URL"),
        None,
        "CURRENT BEHAVIOUR, NOT DESIRED: {GATEWAY} passed every gate this adapter has — \
         non-loopback, https, accepted by base_url_is_acceptable — and was then discarded by the \
         `if spec.auth == Auth::Canned` around the push, so the CLI resolves Google directly with \
         the operator's own login. `bin/marion.rs` now refuses the flag before it reaches here, so \
         this is defence in depth; the absurdity it pins is local and unfixed either way, since \
         this adapter still validates an endpoint it will then ignore. The right answer if it is \
         ever honoured is to push GOOGLE_GEMINI_BASE_URL under Inherited too. When that lands, \
         invert this assertion. Compiled env: {:?}",
        live.env
    );
}

/// **CURRENT BEHAVIOUR, NOT DESIRED.** `CodexAdapter::config_files` returns early on
/// `spec.auth == Auth::Inherited` before the `base_url` is even read, so no `model_providers` block
/// is generated and codex falls back to its own default provider. The early return is right about
/// the *file* — §6.4 forbids marion writing the operator's `~/.codex/config.toml` — but the live
/// route it hands off to (`-c <dotted.key>=<toml>` on argv, `McpRoute::Argv`) carries only the MCP
/// declaration, and `model_providers` would ride it just as well.
#[test]
fn codex_drops_a_gateway_base_url_under_real_auth() {
    let (_, canned) = everything_the_node_is_told(&CodexAdapter, &canned_spec_with_gateway(None));
    assert!(
        canned.contains(GATEWAY_HOST),
        "negative control: under --canned this adapter writes the base URL into the generated \
         model_providers block:\n{canned}"
    );

    let (_, live) = everything_the_node_is_told(&CodexAdapter, &live_spec_with_gateway(None));
    let live = without_the_bridge_declaration(&live);
    assert!(
        !live.contains(GATEWAY_HOST),
        "CURRENT BEHAVIOUR, NOT DESIRED: marion was handed {GATEWAY} and `codex exec` is launched \
         with no model_providers entry at all, so it reaches OpenAI directly with the operator's \
         ~/.codex/auth.json. The CLI now refuses this before it gets here, so this is the layer \
         under that gate. The `-c` argv route this mode already uses for mcp_servers.marion is the \
         channel a gateway would take — writing no file is the §6.4 MUST, writing nothing anywhere \
         is not — so the right answer if it is ever honoured is to emit the provider override on \
         that same route. When that lands, invert this assertion. What the node is told:\n{live}"
    );
}

/// **CURRENT BEHAVIOUR, NOT DESIRED.** `OpenCodeAdapter::config_files` returns early on
/// `spec.auth == Auth::Inherited`, and the live route it hands off to —
/// `OPENCODE_CONFIG_CONTENT`, built by `opencode::live_config_json` — carries the `mcp` block and
/// no `provider` block, so the generated `options.baseURL` that a canned node gets has no live
/// counterpart. `OPENCODE_CONFIG_CONTENT` merges *over* the operator's own config, so it is exactly
/// the channel that could carry one.
#[test]
fn opencode_drops_a_gateway_base_url_under_real_auth() {
    let (_, canned) = everything_the_node_is_told(
        &OpenCodeAdapter,
        &canned_spec_with_gateway(Some("canned/canned-1")),
    );
    assert!(
        canned.contains(GATEWAY_HOST),
        "negative control: under --canned this adapter writes the base URL into the generated \
         provider block's options.baseURL:\n{canned}"
    );

    let (_, live) = everything_the_node_is_told(
        &OpenCodeAdapter,
        &live_spec_with_gateway(Some("acme/some-model")),
    );
    let live = without_the_bridge_declaration(&live);
    assert!(
        !live.contains(GATEWAY_HOST),
        "CURRENT BEHAVIOUR, NOT DESIRED: marion was handed {GATEWAY} and the node's \
         OPENCODE_CONFIG_CONTENT names no provider options at all, so opencode resolves the \
         provider out of the operator's own config and reaches the vendor directly. The CLI now \
         refuses this before it gets here, so this is the layer under that gate. That variable \
         merges last and over the operator's config, which is the property that makes it the right \
         carrier for a deliberate gateway override — so the right answer if it is ever honoured is \
         to emit provider.<id>.options.baseURL there. When that lands, invert this assertion. What \
         the node is told:\n{live}"
    );
}

// ---------------------------------------------------------------------------------------------
// Where the endpoint does travel — and dies.
// ---------------------------------------------------------------------------------------------

/// **The gateway is not lost at the adapter seam; it is carried past it and then discarded.**
///
/// `root::compile` copies `--base-url` into `LaunchSpec.base_url` in both modes, and every
/// adapter's `BridgeEnv` copies that into the child's `MARION_BASE_URL`. So the URL really is
/// present in the declaration a live node's bridge reads — and `main::auth_from_env` then maps
/// `Inherited` to `base_url: None` regardless, by design (a live child is overlaid no endpoint,
/// exactly as its parent is).
///
/// Kept because it fences off the wrong fix. Deleting `MARION_BASE_URL` from the live declaration
/// would look like tidying a dead value and would in fact remove the only place a gateway survives
/// the compile at all — the one thread a real implementation would pull on.
#[test]
fn the_gateway_reaches_the_child_bridge_declaration_and_is_dropped_there_instead() {
    let (_, blob) =
        everything_the_node_is_told(&ClaudeCodeAdapter, &live_spec_with_gateway(Some("haiku")));
    assert!(
        blob.contains(&format!("\"MARION_BASE_URL\": \"{GATEWAY}\"")),
        "the live node's --mcp-config declaration carries MARION_BASE_URL verbatim; if this stops \
         being true the gateway no longer survives compile anywhere:\n{blob}"
    );
    assert!(
        !without_the_bridge_declaration(&blob).contains(GATEWAY_HOST),
        "and that is its only appearance: the harness itself is told nothing about the gateway"
    );
}

// ---------------------------------------------------------------------------------------------
// `MARION_AUTH` across a spawn hop: a live root's child is live, on every route.
//
// The bridge serves a node marion did not start, so the only channel from a live root to the
// child it spawns is the per-server `env` block marion wrote into that root's declaration. These
// tests are the writing half; `main_tests`' `auth_from_env` tests are the reading half. Split
// because `auth_from_env` is private to the `marion-supervisor` binary and cannot be called from
// an integration test — not because a hop is unprovable without launching. Nothing here starts a
// process.
//
// **Neither half spells the token itself.** Two halves that meet at a hand-typed `"inherited"`
// drift silently: rename the spelling on one side and both tests keep passing while the hop
// breaks. So every needle below is *built* from `Auth::Inherited.as_wire()`, which is the same
// function all four adapters serialise through, and the reading half calls `auth_from_env` with
// that same value — one source, both ends. `the_wire_spelling_round_trips` ties the last knot; see
// its comment for what is genuinely unpinned today.
//
// Each test also asserts `MARION_BASE_URL` is **absent, not empty**. That is a real regression,
// not a hypothetical: `unwrap_or_default()` once wrote `MARION_BASE_URL: ""` into a live root's
// declaration, the bridge read it back as `Ok("")`, and the child was compiled canned against an
// endpoint spelled as the empty string — neither live nor working, with nothing anywhere
// reporting it. The design names the class "endpoint-as-mode conflation": the mode is a *stated*
// decision, never inferred from a URL's presence or absence.
//
// Four routes, four tests, never a loop: the declaration is a JSON document on claude, repeated
// `-c` argv on codex, a settings JSON on gemini and a JSON-in-an-env-var on opencode. A fix to
// one is not a fix to the others, and a loop would hide three failures behind the first.
// ---------------------------------------------------------------------------------------------

/// A live root, as `marion run <type>` builds one: real auth and **no endpoint at all**, since
/// marion overlays none under `Inherited`.
fn live_spec_without_endpoint(model: Option<&str>) -> LaunchSpec {
    LaunchSpec {
        base_url: None,
        ..live_spec_with_gateway(model)
    }
}

/// **The one knot the two halves are tied with.**
///
/// `Auth::as_wire` and `Auth::from_wire` are two *independent* match tables sitting next to each
/// other in `adapter.rs`, and nothing in the workspace asserted they are inverses. Every writer
/// goes through the first (`claude_code`, `codex`, `gemini` and `opencode` all serialise
/// `auth.as_wire()`) and the only reader goes through the second (`main::auth_from_env`), so an
/// edit to one table alone breaks the hop — and does it in the *silent* direction, since
/// `from_wire` failing to recognise a value falls back to `Canned` rather than refusing.
///
/// That is what makes this the tie rather than a tautology: it is the only assertion anywhere that
/// fails on such an edit, and the declaration tests below take their needle from `as_wire` so the
/// same rename cannot leave them passing against a stale literal.
#[test]
fn the_wire_spelling_round_trips_so_the_two_halves_cannot_drift_apart() {
    for mode in [Auth::Canned, Auth::Inherited] {
        assert_eq!(
            Auth::from_wire(mode.as_wire()),
            Some(mode),
            "{mode:?} serialises as {:?} and reads back as something else. as_wire and from_wire \
             are two separate match tables: a rename in one is a live root whose child is silently \
             canned, because an unrecognised value falls back to Canned rather than refusing",
            mode.as_wire()
        );
    }
}

/// The `MARION_AUTH=inherited` needle in one route's own quoting, assembled from the two names
/// that are actually load-bearing rather than typed out: `root::AUTH_ENV` is the constant
/// `main::spawn_env` reads the variable by, and `as_wire` is the function all four adapters
/// serialise the mode through. Only the *quoting* differs per route, and that is the caller's.
fn auth_needle(quote: fn(&str, &str) -> String) -> String {
    quote(marion_supervisor::root::AUTH_ENV, Auth::Inherited.as_wire())
}

/// A pretty-printed `serde_json` object member, which is how claude's `--mcp-config` document and
/// gemini's settings document both carry it.
fn json_member(k: &str, v: &str) -> String {
    format!("\"{k}\": \"{v}\"")
}

/// The declaration must say the mode outright. Absence would be read back as `Canned`
/// (`Auth::from_wire` returning `None` and the caller defaulting), and a canned child of a live
/// root launches against marion's canned server, which under real auth is not running.
fn assert_declares_inherited_and_no_endpoint(harness: &str, blob: &str, auth_needle: &str) {
    assert!(
        blob.contains(auth_needle),
        "{harness}: the declaration does not carry {auth_needle}, so this live root's child would \
         be compiled canned and launched against a canned server that is not running:\n{blob}"
    );
    assert!(
        !blob.contains(marion_supervisor::root::BASE_URL_ENV),
        "{harness}: a live declaration must omit {} entirely, never write it empty — an empty \
         value read back as Ok(\"\") produced a child that was neither live nor canned. This \
         asserts the key is ABSENT, not that its value is falsy:\n{blob}",
        marion_supervisor::root::BASE_URL_ENV
    );
}

/// The route: the `--mcp-config` document, whose declaration block is JSON.
#[test]
fn a_live_claude_root_declares_its_child_inherited() {
    let (_, blob) = everything_the_node_is_told(
        &ClaudeCodeAdapter,
        &live_spec_without_endpoint(Some("haiku")),
    );
    assert_declares_inherited_and_no_endpoint("claude", &blob, &auth_needle(json_member));
}

/// The route: repeated `-c mcp_servers.marion.<key>=<toml>` on argv — neither a document nor an
/// environment variable, because §6.4 forbids marion writing the `config.toml` a live codex node
/// reads.
#[test]
fn a_live_codex_root_declares_its_child_inherited() {
    let (_, blob) = everything_the_node_is_told(&CodexAdapter, &live_spec_without_endpoint(None));
    // A TOML scalar inside a `-c` argument, seen through the `{:?}` the blob is built with — so
    // the inner quotes arrive escaped.
    let needle = auth_needle(|k, v| format!("{k}=\\\"{v}\\\""));
    assert_declares_inherited_and_no_endpoint("codex", &blob, &needle);
}

/// The route: the system-settings JSON that `GEMINI_CLI_SYSTEM_SETTINGS_PATH` names.
#[test]
fn a_live_gemini_root_declares_its_child_inherited() {
    let (_, blob) = everything_the_node_is_told(
        &GeminiAdapter,
        &live_spec_without_endpoint(Some("gemini-2.5-flash")),
    );
    assert_declares_inherited_and_no_endpoint("gemini", &blob, &auth_needle(json_member));
}

/// The route: `OPENCODE_CONFIG_CONTENT`, a JSON document inline in the child's environment.
#[test]
fn a_live_opencode_root_declares_its_child_inherited() {
    let (inv, _) = everything_the_node_is_told(
        &OpenCodeAdapter,
        &live_spec_without_endpoint(Some("acme/some-model")),
    );
    let content = env_value(&inv, "OPENCODE_CONFIG_CONTENT")
        .expect("a live opencode node's declaration rides OPENCODE_CONFIG_CONTENT");
    // Compact, not pretty: this document is serialised into an environment variable.
    let needle = auth_needle(|k, v| format!("\"{k}\":\"{v}\""));
    assert_declares_inherited_and_no_endpoint("opencode", content, &needle);
}
