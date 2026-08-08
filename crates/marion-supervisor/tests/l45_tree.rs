//! **L4.5 for the tree screen** (§8) — `TestBackend` + `insta`, over the *production* projection.
//!
//! # Why this target is here and not in `marion-tui`
//!
//! `marion-term/tests/l45_driver.rs` is the other half of this level and it renders a grid. This
//! one renders §5.6's tree pane, and the thing worth snapshotting about a tree pane is **which
//! actions are greyed** — which `marion-tui` cannot decide, by construction: it has no dependency
//! that could reach a capability table (see `marion_tui::tree`'s module doc). A snapshot taken over
//! there would be a snapshot of availability bits a test made up.
//!
//! So the driver lives in the crate that *does* the deciding, and every cell below comes off the
//! path `marion tree` actually runs: [`marion_supervisor::tree::build`] over real
//! [`NodeSummary`]s, greyed by `doctor::capabilities_at`, drawn by `marion_tui::tree`'s widgets
//! through `ratatui`. The only thing standing in for the supervisor is the `Vec<NodeSummary>` a
//! `tree/subscribe` would have returned.
//!
//! # What the snapshot carries, and why the style census is not optional
//!
//! Greying is a **style**, and the label text is deliberately identical either way — see
//! `marion_tui::tree`'s "two styles, and no third". A snapshot of the characters alone would render
//! a completely mis-greyed action bar identically to a correct one. The census below is therefore
//! the load-bearing half of the payload, and the plain rows are the readable half.

use marion_core::contract::AgentId;
use marion_core::encoding::Duration;
use marion_core::harness::Harness;
use marion_core::node::{BlockReason, NodeState, ReapState};
use marion_proto::NodeSummary;
use marion_supervisor::tree::build;
use marion_tui::tree::{self, ActionBar, Tree, TreeView};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

const COLS: u16 = 96;
const ROWS: u16 = 12;

fn node(
    id: &str,
    parent: Option<&str>,
    harness: Harness,
    version: Option<&str>,
    pane: bool,
    state: NodeState,
) -> NodeSummary {
    NodeSummary {
        agent_id: AgentId(id.into()),
        parent_id: parent.map(|p| AgentId(p.into())),
        name: None,
        agent_type: "codex-impl".into(),
        harness,
        harness_version: version.map(str::to_string),
        depth: 0,
        state,
        reap_state: ReapState::Live,
        timeout: Duration::from_secs(900),
        pane,
    }
}

/// A forest that exercises every distinction the greying is keyed on.
///
/// Not decorative: each entry is a row of §3.3's key that answers differently from its neighbour,
/// so a snapshot that stopped depending on one component of the key would visibly change.
fn forest() -> Vec<NodeSummary> {
    vec![
        // claude-code headless: `Typed(StreamJson)`, so `permissions` publishes (S9).
        node(
            "root-claude",
            None,
            Harness::ClaudeCode,
            Some("2.1.223"),
            false,
            NodeState::Running,
        ),
        // The same binary on a pane: §3.4's `opaque` clips `permissions` and keeps `interrupt`.
        node(
            "pane-claude",
            Some("root-claude"),
            Harness::ClaudeCode,
            Some("2.1.223"),
            true,
            NodeState::Idle,
        ),
        // codex on its shipped `LaunchOnly` surface: nothing publishes, whatever the version says.
        node(
            "codex-146",
            Some("root-claude"),
            Harness::Codex,
            Some("0.146.0"),
            false,
            NodeState::Blocked(BlockReason::Permission),
        ),
        // A node still spawning has no `Spawned` record and therefore no version — the
        // conservative key, which is the one §3.3's *degrade visibly* asks for.
        node(
            "codex-unversioned",
            Some("codex-146"),
            Harness::Codex,
            None,
            false,
            NodeState::Spawning,
        ),
        // An orphan: its parent is not in this snapshot (§4.2's compaction). It must still be a row.
        node(
            "orphan-gemini",
            Some("compacted-away"),
            Harness::Gemini,
            Some("0.53.0"),
            false,
            NodeState::Exited(marion_core::contract::ExitStatus::Ok),
        ),
    ]
}

fn draw(tree: &Tree, focused: bool) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(COLS, ROWS)).expect("TestBackend");
    terminal
        .draw(|f| {
            let panes = tree::split(f.area());
            f.render_widget(TreeView { tree, focused }, panes.tree);
            f.render_widget(
                ActionBar {
                    node: tree.selected(),
                },
                panes.actions,
            );
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

fn rows_of(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let mut row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            row.truncate(row.trim_end().len());
            row
        })
        .collect()
}

/// The tree column, each row prefixed with the style its first cell carries.
///
/// **The prefix is why the focused and blurred snapshots are two snapshots.** Focus is drawn as
/// `REVERSED` on the cursor row and nothing else; the *characters* are identical either way, so a
/// text-only payload would make one of the two a copy of the other and neither would be testing the
/// focus model at all.
fn tree_rows(buf: &Buffer) -> String {
    let width = tree::TREE_COLUMN.min(buf.area.width);
    (0..buf.area.height.saturating_sub(1))
        .map(|y| {
            let mut text: String = (0..width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            text.truncate(text.trim_end().len());
            format!("{:?}  {text:?}", buf[(0, y)].style())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The greying, cell by cell, as one line per contiguous run of style.
///
/// This is the assertion the characters cannot make. A run is `<style> "text"`, so a swap of two
/// actions' availability moves text between two lines and a wholesale loss of greying collapses the
/// bar to a single run.
fn strip_runs(buf: &Buffer) -> String {
    let y = buf.area.height - 1;
    let mut out = String::new();
    let mut run = String::new();
    let mut style = None;
    for x in 0..buf.area.width {
        let cell = &buf[(x, y)];
        let s = format!("{:?}", cell.style());
        if style.as_ref() != Some(&s) {
            if let Some(prev) = style.take() {
                out.push_str(&format!("{prev}  {run:?}\n"));
            }
            run.clear();
            style = Some(s);
        }
        run.push_str(cell.symbol());
    }
    if let Some(prev) = style {
        out.push_str(&format!("{prev}  {run:?}\n"));
    }
    out
}

fn report(tree: &Tree, focused: bool) -> String {
    let buf = draw(tree, focused);
    format!(
        "--- {}x{}, tree {} ---\n{}\n\
         --- tree column, one line per row, styled ---\n{}\n\
         --- action strip, one line per style run ---\n{}",
        COLS,
        ROWS,
        if focused { "focused" } else { "blurred" },
        rows_of(&buf).join("\n"),
        tree_rows(&buf),
        strip_runs(&buf),
    )
}

// ---------------------------------------------------------------------------------------------
// The snapshots.
// ---------------------------------------------------------------------------------------------

/// The forest as it first paints: cursor on the root, whose harness publishes two capabilities.
#[test]
fn l45_tree_root_selected() {
    let tree = build(&forest(), None);
    insta::assert_snapshot!(report(&tree, true));
}

/// **The clause-3 snapshot.** The cursor moves to the *pane* node of the same harness and the same
/// version, and `permissions` goes from offered to greyed with nothing else changing. One harness,
/// one binary, two surface keys, two answers — which is §9's M1 sentence said about claude-code.
#[test]
fn l45_tree_pane_node_greys_what_its_surfaces_cannot_carry() {
    let mut tree = build(&forest(), None);
    tree.move_by(1);
    assert_eq!(tree.selected().unwrap().id, "pane-claude");
    insta::assert_snapshot!(report(&tree, true));
}

/// A `LaunchOnly` node: §3.4 gives it no channel after argv, so every control capability is greyed
/// and the whole bar is one run. This is the row that would look identical to a *broken* bar, which
/// is why the two above are snapshotted beside it.
#[test]
fn l45_tree_a_launch_only_node_greys_everything() {
    let mut tree = build(&forest(), None);
    tree.move_by(2);
    assert_eq!(tree.selected().unwrap().id, "codex-146");
    insta::assert_snapshot!(report(&tree, true));
}

/// The tree without the keyboard. §5.6's two panes are two panes only if an operator can see which
/// one `j` goes to, and the difference from [`l45_tree_root_selected`] is one row's style — which
/// is exactly what `tree_rows` puts in the payload and a text-only snapshot would have lost.
#[test]
fn l45_tree_blurred() {
    let tree = build(&forest(), None);
    insta::assert_snapshot!(report(&tree, false));
}

/// An empty forest is a real answer — a supervisor with no nodes — and must render as a sentence
/// rather than as a blank screen an operator reads as a hang.
#[test]
fn l45_tree_empty() {
    let tree = build(&[], None);
    insta::assert_snapshot!(report(&tree, true));
}

// ---------------------------------------------------------------------------------------------
// Properties that stop the snapshots above from being vacuous.
// ---------------------------------------------------------------------------------------------

/// **Every node in the snapshot is on the screen.** A `tree/subscribe` result whose rows outnumber
/// the painted lines is the "tree omitting a live node" failure, and it would pass every snapshot
/// above by simply never appearing in one.
#[test]
fn no_node_is_missing_from_the_painted_tree() {
    let nodes = forest();
    let tree = build(&nodes, None);
    let buf = draw(&tree, true);
    let painted = rows_of(&buf).join("\n");
    for n in &nodes {
        assert!(
            painted.contains(&n.agent_id.0),
            "`{}` is in the supervisor's answer and not on the screen:\n{painted}",
            n.agent_id.0
        );
    }
    assert_eq!(tree.rows(), nodes.len());
}

/// **The snapshots differ from each other, and differ in the strip.** Two rows that rendered
/// identically would mean the cursor changed nothing, and the whole clause-3 story above rests on
/// the strip moving when the selection does.
#[test]
fn moving_the_cursor_changes_the_action_strip() {
    let mut tree = build(&forest(), None);
    let head = strip_runs(&draw(&tree, true));
    tree.move_by(1);
    let pane = strip_runs(&draw(&tree, true));
    assert_ne!(
        head, pane,
        "the headless and pane rows of one harness rendered the same strip, so the surface \
         component of §3.3's key reached nothing"
    );
    // And specifically: `permissions` is offered on one and greyed on the other.
    let greyed_permissions = |s: &str| {
        s.lines()
            .any(|l| l.contains("permissions") && l.contains("crossed_out"))
    };
    assert!(!greyed_permissions(&head), "{head}");
    assert!(greyed_permissions(&pane), "{pane}");
}

/// The focused and blurred snapshots must not be copies of each other.
///
/// They render identical *characters* by design, so without this the pair would be two reviews of
/// one payload and the focus model would be snapshotted by nothing.
#[test]
fn focus_is_visible_in_the_payload() {
    let tree = build(&forest(), None);
    assert_ne!(
        report(&tree, true),
        report(&tree, false),
        "focused and blurred rendered the same payload, so the focus model is untested"
    );
}

/// `.githooks/pre-commit` must name **this** target too, and expect this many tests.
///
/// The same latch `marion-term`'s `the_gate_names_this_target_and_only_this_target` installs, for
/// the same reason: the hook reads back a pass count so it cannot silently no-op, and that check is
/// only as good as its threshold. Renaming this target or deleting a test fails here rather than
/// passing vacuously.
#[test]
fn the_gate_names_this_target_too() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script =
        std::fs::read_to_string(root.join(".githooks/pre-commit")).expect("the hook must exist");
    assert!(
        script.contains("TARGET_TREE=l45_tree"),
        "the hook does not name this target, so the tree UI is outside the L4.5 gate"
    );
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/l45_tree.rs"),
    )
    .expect("this file");
    let tests = source.matches("\n#[test]\n").count();
    let declared: usize = script
        .lines()
        .find_map(|l| l.strip_prefix("MIN_TESTS_TREE="))
        .expect("MIN_TESTS_TREE")
        .trim()
        .parse()
        .expect("MIN_TESTS_TREE is a number");
    assert_eq!(
        declared, tests,
        "the hook expects {declared} tests and this file defines {tests}"
    );
}
