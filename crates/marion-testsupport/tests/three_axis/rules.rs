//! Each rule against a hand-written snippet: what it must flag, and the near-misses it must not.
//! These pin the detectors themselves; the workspace tests above pin the tree.

use crate::scan::{self, Axis, FileFacts, Vocabulary};
use crate::walk::Source;

fn vocab() -> Vocabulary {
    Vocabulary {
        variants: ["ClaudeCode", "Codex", "Acp"].map(String::from).to_vec(),
        names: ["claude", "claude-code", "codex", "acp"]
            .map(String::from)
            .to_vec(),
        words: ["Claude", "Codex"].map(String::from).to_vec(),
    }
}

fn scan_with(src: &str, test_file: bool, row_file: bool) -> Vec<(Axis, &'static str)> {
    let ast = syn::parse_file(src).expect("snippet parses");
    let secret_docs = crate::security::writes_secret_docs(&ast);
    let source = Source {
        rel: "x.rs".to_string(),
        ast,
        test_file,
    };
    scan::scan(
        &source,
        &vocab(),
        FileFacts {
            row_file,
            secret_docs,
        },
    )
    .into_iter()
    .map(|f| (f.axis, f.rule))
    .collect()
}

fn rules(src: &str) -> Vec<&'static str> {
    scan_with(src, false, false)
        .into_iter()
        .map(|(_, r)| r)
        .collect()
}

// ---- generality -------------------------------------------------------------------------------

#[test]
fn a_harness_variant_pattern_is_flagged_in_match_if_let_and_matches() {
    assert!(
        rules("fn f(h: Harness) { match h { Harness::Codex => {}, _ => {} } }")
            .contains(&"harness-variant-pattern")
    );
    assert!(
        rules("fn f(h: Option<Harness>) { if let Some(Harness::Acp) = h {} }")
            .contains(&"harness-variant-pattern")
    );
    assert!(
        rules("fn f(h: Harness) -> bool { matches!(h, Harness::Codex | Harness::Acp) }")
            .contains(&"harness-variant-pattern")
    );
    assert!(
        rules("fn f(h: Harness) -> bool { marion_core::Harness::Codex == h }")
            .contains(&"harness-variant-compare")
    );
    assert!(
        rules("fn f(h: Harness) -> bool { [Harness::Codex].contains(&h) }")
            .contains(&"harness-variant-compare")
    );
}

#[test]
fn constructing_a_harness_or_matching_another_enum_is_not_flagged() {
    assert!(rules("fn f() -> Harness { Harness::Codex }").is_empty());
    assert!(rules("fn f(m: Mode) { match m { Mode::Plain => {}, _ => {} } }").is_empty());
    assert!(rules("fn f() { let all = Harness::ALL; }").is_empty());
}

#[test]
fn harness_name_literals_are_flagged_only_in_conditionals() {
    assert!(rules(r#"fn f(s: &str) -> bool { s == "codex" }"#).contains(&"harness-name-compare"));
    assert!(
        rules(r#"fn f(s: &str) { match s { "claude" => {}, _ => {} } }"#)
            .contains(&"harness-name-pattern")
    );
    assert!(
        rules(r#"fn f(s: &str) -> bool { s.starts_with("acp") }"#)
            .contains(&"harness-name-compare")
    );
    assert!(rules(r#"fn f() -> Command { Command::new("codex") }"#).is_empty());
    assert!(rules(r#"fn f(s: &str) -> bool { s == "coder" }"#).is_empty());
}

#[test]
fn agent_type_names_built_on_a_harness_are_flagged_anywhere() {
    assert!(rules(r#"fn f() -> &'static str { "codex-impl" }"#).contains(&"agent-type-literal"));
    assert!(rules(r#"fn f() -> &'static str { "claude-code" }"#).is_empty());
    assert!(rules(r#"fn f() -> &'static str { "codex impl" }"#).is_empty());
}

#[test]
fn harness_named_impls_variants_constants_and_env_are_flagged() {
    let r = rules(
        "impl HarnessAdapter for CodexAdapter { fn f(h: Harness) { match h { Harness::Codex => {} } } }",
    );
    assert_eq!(
        r,
        vec!["harness-impl-outside-row"],
        "one finding for the block, none inside"
    );
    assert!(
        rules("fn f(p: Push) { match p { Push::ClaudeChannel => {}, _ => {} } }")
            .contains(&"harness-named-variant")
    );
    assert!(
        rules("fn f(s: &str) -> bool { s.starts_with(ACP_COMMAND_PREFIX) }")
            .contains(&"harness-name-compare")
    );
    assert!(
        rules(r#"fn f() { env.push(("ANTHROPIC_AUTH_TOKEN", t)); }"#)
            .contains(&"vendor-env-literal")
    );
    // `Acp` is the shared protocol, `Codexy` is not at a word boundary, lower-case is a value.
    assert!(rules("impl HarnessAdapter for AcpAdapter {}").is_empty());
    assert!(rules("fn f(p: P) { match p { P::Codexy => {} } }").is_empty());
    assert!(rules("fn f(s: &str) -> bool { s == acp_prefix }").is_empty());
}

#[test]
fn row_files_and_test_code_are_exempt_from_generality() {
    let src = "fn f(h: Harness) { match h { Harness::Codex => {}, _ => {} } }";
    assert!(scan_with(src, false, true).is_empty());
    assert!(scan_with(src, true, false).is_empty());
    assert!(rules(&format!("#[cfg(test)] mod tests {{ {src} }}")).is_empty());
    assert!(!rules(&format!("#[cfg(not(test))] mod live {{ {src} }}")).is_empty());
}

#[test]
fn doc_comments_are_not_code() {
    assert!(
        rules("/// `Harness::Codex` and \"codex-impl\" and thread::sleep in a loop\nfn f() {}")
            .is_empty()
    );
}

// ---- efficiency -------------------------------------------------------------------------------

#[test]
fn sleep_and_polls_inside_loops_are_flagged() {
    assert!(rules("fn f() { loop { std::thread::sleep(D); } }").contains(&"sleep-in-loop"));
    assert!(rules("fn f() { while x { thread::sleep(D) } }").contains(&"sleep-in-loop"));
    assert!(rules("fn f() { for _ in 0..3 { rx.recv_timeout(POLL); } }").contains(&"poll-in-loop"));
    assert!(
        rules("fn f() { loop { rx.recv_timeout(left.min(Duration::from_millis(5))); } }")
            .contains(&"poll-in-loop")
    );
    assert!(
        rules("fn f() { loop { if c.try_wait().is_ok() { break } } }").contains(&"poll-in-loop")
    );
    assert!(rules("fn f() { loop { std::thread::yield_now(); } }").contains(&"busy-loop"));
}

#[test]
fn one_shot_waits_outside_loops_are_not_flagged() {
    assert!(rules("fn f() { std::thread::sleep(D); rx.recv_timeout(D); }").is_empty());
    // The condvar spurious-wakeup loop, waiting only for the time left, is a bounded wait.
    assert!(
        rules("fn f() { loop { let left = deadline - now(); cv.wait_timeout(g, left); } }")
            .is_empty()
    );
    // A loop in an enclosing fn does not reach a nested fn's body.
    assert!(rules("fn f() { loop { fn g() { std::thread::sleep(D) } } }").is_empty());
}

#[test]
fn read_timeouts_and_named_poll_periods_are_flagged() {
    assert!(rules("fn f(s: S) { s.set_read_timeout(Some(POLL)); }").contains(&"read-timeout-tick"));
    assert!(
        rules("fn f(s: S) { s.set_read_timeout(Some(Duration::from_millis(50))); }")
            .contains(&"read-timeout-tick")
    );
    assert!(rules("fn f(s: S) { s.set_read_timeout(None); }").is_empty());
    assert!(rules("fn f(s: S) { s.set_read_timeout(Some(deadline.remaining())); }").is_empty());
    assert!(
        rules("const PANE_POLL: Duration = Duration::from_millis(20);")
            .contains(&"named-poll-period")
    );
    assert!(rules("const GRACE: Duration = Duration::from_millis(20);").is_empty());
    assert!(rules("const POLLIN: i16 = 0x0001; const TICKET_ATTEMPTS: u32 = 8;").is_empty());
    assert!(rules("const DRAIN_POLL_MS: u64 = 20;").contains(&"named-poll-period"));
}

// ---- security ---------------------------------------------------------------------------------

#[test]
fn secret_types_and_fields_must_not_derive_debug() {
    assert!(rules("#[derive(Debug)] struct ApiKey(String);").contains(&"secret-debug"));
    assert!(
        rules("#[derive(Clone, Debug)] struct Endpoint { url: String, api_key: String }")
            .contains(&"secret-debug")
    );
    assert!(
        rules("#[derive(Debug)] enum Auth { Endpoint { token: String } }")
            .contains(&"secret-debug")
    );
    assert!(
        rules("struct Token(String); impl fmt::Display for Token { fn fmt(&self) {} }")
            .contains(&"secret-debug")
    );
    // Hand-written Debug is the fix; a key *event* holds no string; no Debug, no leak.
    assert!(
        rules("struct ApiKey(String); impl fmt::Debug for ApiKey { fn fmt(&self) {} }").is_empty()
    );
    assert!(rules("#[derive(Debug)] struct KeyEvent { code: u32 }").is_empty());
    assert!(rules("#[derive(Debug)] struct CredentialId { provider: String }").is_empty());
    assert!(rules("#[derive(Debug)] struct Env { key: String, value: String }").is_empty());
    assert!(rules("struct Endpoint { api_key: String }").is_empty());
}

#[test]
fn formatting_a_secret_is_flagged_but_its_presence_is_not() {
    assert!(rules(r#"fn f() { eprintln!("{}", e.api_key); }"#).contains(&"secret-format"));
    assert!(rules(r#"fn f() { let s = format!("key={token}"); }"#).contains(&"secret-format"));
    assert!(rules(r#"fn f() { tracing::info!(api_key = %k, "x"); }"#).contains(&"secret-format"));
    assert!(
        rules(r#"fn f() { let l = format!("-w {}", key.expose()); }"#).contains(&"secret-format")
    );
    // An environment variable's name is not its value.
    assert!(rules(r#"fn f() { eprintln!("{key} is unset"); }"#).is_empty());
    assert!(rules(r#"fn f() { eprintln!("{}", token.is_some()); }"#).is_empty());
    assert!(rules(r#"fn f() { eprintln!("token usage {n}"); }"#).is_empty());
    assert!(rules(r#"fn redact(t: &str) -> String { format!("{}…", &token[..4]) }"#).is_empty());
}

#[test]
fn secrets_on_argv_are_flagged() {
    assert!(rules("fn f() { cmd.arg(api_key); }").contains(&"secret-argv"));
    assert!(rules(r#"fn f() { cmd.args(["--api-key", k]); }"#).contains(&"secret-argv"));
    assert!(rules(r#"fn f() { cmd.arg(format!("--token={}", t)); }"#).contains(&"secret-argv"));
    assert!(rules(r#"fn f() { cmd.arg("--model").env("API_KEY", k); }"#).is_empty());
}

#[test]
fn credential_docs_need_an_owner_only_mode() {
    let unsafe_write = r#"fn f() { std::fs::write(dir.join("mcp.json"), doc); }"#;
    assert!(rules(unsafe_write).contains(&"secret-doc-mode"));
    let safe = r#"fn f() { let p = "mcp.json"; OpenOptions::new().write(true).create(true).mode(0o600).open(p); }"#;
    assert!(!rules(safe).contains(&"secret-doc-mode"));
    // A module with no credential-bearing document is not asked for a mode.
    assert!(rules(r#"fn f() { std::fs::write("notes.txt", s); }"#).is_empty());
}

#[test]
fn tests_never_spell_a_login_flow() {
    let t = |src: &str| scan_with(src, true, false);
    assert!(t(r#"fn t() { cmd.arg("login"); }"#).contains(&(Axis::Security, "test-login")));
    assert!(t(r#"fn t() { run("gemini auth login"); }"#).contains(&(Axis::Security, "test-login")));
    assert!(t(r#"fn t() { send("/login"); }"#).contains(&(Axis::Security, "test-login")));
    assert!(t(r#"fn t() { assert!(out.contains("run `claude` to log in")); }"#).is_empty());
    assert!(t(r#"fn t() { assert!(msg.contains("marion login openai")); }"#).is_empty());
    assert!(t(r#"fn t() { skip("runs on the operator's own login (spends quota)"); }"#).is_empty());
    // Production code naming a login is not a test starting one.
    assert!(rules(r#"fn f() { hint("run `codex login`"); }"#).is_empty());
}
