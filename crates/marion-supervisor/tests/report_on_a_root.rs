//! **A root calls `report`, and marion's own bridge refuses it** — §5.4's authorization table, at
//! the one point every harness passes through.
//!
//! §5.4 of the design says of `report`: *"self only, and only on a node that has a contract —
//! rejected on a root."* Until this file existed nothing enforced that sentence anywhere a root
//! could reach:
//!
//! * `bridge::tools()` declares `report` to **every** node, root included, and says so in its own
//!   doc comment (deliberately — a merely-absent verb carries no bound and no sentence);
//! * `root::ROOT_ALLOWED_TOOLS` omits it, but that is the **permission** axis, and only the Claude
//!   Code adapter compiles a per-tool permission list at all (`run.rs`'s note on `check_spawn_gates`
//!   — codex, gemini and opencode compile none);
//! * so on the three `LaunchOnly` harnesses a root's `report` arrived at `marion-supervisor mcp` and
//!   was answered `report recorded`, `isError: false`. The payload was discarded — nothing stages a
//!   root's report, because there is no contract to stage it into — and the root was told it had
//!   succeeded. A root that reports instead of delegating therefore exits `Ok` having done nothing,
//!   which is §12's silent-failure shape exactly.
//!
//! # Why this drives the bridge binary rather than a harness
//!
//! The refusal lives at the **execution** point (`main::handle_tool_call`), which is the one place
//! all four harnesses share: a `LaunchOnly` harness's node speaks JSON-RPC to this binary over
//! stdio, and the request written below is byte-for-byte what such a node's MCP client sends. Going
//! through a real `codex` would test codex's MCP client on top of the rule, at three times the cost,
//! and would leave gemini and opencode uncovered unless it were done three times over.
//!
//! The node's depth rides the per-server `env` block marion wrote into the declaration
//! (`marion_harness::adapter`'s own test sweeps `Harness::ALL` for `MARION_DEPTH`), which is why it
//! is set here as an environment variable and not as an argument: this is the channel production
//! uses.
//!
//! ```sh
//! cargo test -p marion-supervisor --test report_on_a_root
//! ```
//!
//! Nothing is launched but marion's own bridge: no harness binary, no model, no provider.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use marion_supervisor::root::{AGENT_ID_ENV, AGENT_TYPE_ENV, DEPTH_ENV, ROOT_DEPTH};
use serde_json::{Value, json};

/// One `tools/call` against a bridge told it is serving a node at `depth`, answered.
///
/// The bridge dispatches per line and holds no session state, so one request is a complete
/// conversation — `initialize` would change nothing about the answer and is left out rather than
/// performed for decoration.
fn call_report(depth: u32) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_marion-supervisor"))
        .arg("mcp")
        // The declaration marion writes for every node, in the three keys this answer reads.
        .env(AGENT_ID_ENV, "019f-report-probe")
        .env(AGENT_TYPE_ENV, "codex")
        .env(DEPTH_ENV, depth.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("marion's own bridge starts");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "report",
            "arguments": {"narrative": "the root reports instead of delegating"},
        },
    });
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();

    let mut lines = BufReader::new(stdout).lines();
    let reply = lines
        .next()
        .expect("the bridge answers every tools/call")
        .expect("the answer is a line");
    // Closing stdin is what ends a stdio MCP session.
    drop(stdin);
    let _ = child.wait();
    serde_json::from_str(&reply).expect("the answer is one JSON-RPC frame")
}

fn text(v: &Value) -> String {
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// **The defect, at the depth §5.4 names.**
///
/// Three assertions, each of which fails against the pre-fix bridge, which answered
/// `{"text": "report recorded", "isError": false}`:
///
/// 1. it is an **error result** at all, so the root learns the call did not happen;
/// 2. it says **why** in §5.4's terms — self-only, requires a contract, and this node is a root —
///    because a refusal a node cannot act on is barely better than the false success it replaced;
/// 3. it does **not** say `report recorded`, which is the sentence that made the lie.
#[test]
fn a_roots_report_is_refused_rather_than_answered_report_recorded() {
    let v = call_report(ROOT_DEPTH);
    assert_eq!(
        v["result"]["isError"],
        json!(true),
        "§5.4 rejects `report` on a root: a node with no contract that is told its report was \
         recorded exits Ok having delegated nothing — {v}"
    );
    let text = text(&v);
    assert!(
        !text.contains("report recorded"),
        "the false receipt is what this refusal replaces: {text}"
    );
    for needle in ["§5.4", "contract", "root"] {
        assert!(
            text.contains(needle),
            "the refusal must name the rule it is enforcing ({needle:?} missing): {text}"
        );
    }
}

/// **The half a careless refusal breaks.** `report` is a child's whole return path: a node at any
/// depth below the root has a contract, so the same call is the verb working exactly as designed.
/// A depth check that read "is the depth known" rather than "is it zero" would pass the test above
/// and fail here.
#[test]
fn a_childs_report_is_still_recorded() {
    let v = call_report(ROOT_DEPTH + 1);
    assert_eq!(
        v["result"]["isError"],
        json!(false),
        "a child has a contract and `report` is its return path (§5.4): {v}"
    );
    assert_eq!(text(&v), "report recorded");
}
