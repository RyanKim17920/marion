//! marion-supervisor as a library, so the two binaries share one implementation.
//!
//! There are two binaries in this crate and they are deliberately not two crates:
//!
//! - `marion-supervisor`, whose `mcp` subcommand is the stdio MCP bridge a harness starts (§10);
//! - `marion`, the user-facing command, whose `run` subcommand launches the root node (§9).
//!
//! §10's table says the socket moves from the `marion` process to a detached `marion-supervisor`
//! at M2. M1 deliberately has **no socket at all** — "no daemon" means no *detached* supervisor,
//! and the split is M2's job — so the registry and the spawn path are simply linked into whichever
//! binary needs them. Keeping them in one crate is what makes that true today: `marion run` and
//! `marion-supervisor mcp` call the *same* `run::run_spawn`, not two copies that could drift while
//! the socket is not yet there to force agreement.

/// §5.4's `background`, honoured: the handles this bridge has handed out, and which node each one
/// is about. Since §11 item 28 step 5 it owns no child and runs no thread.
/// `marion attach <agent-id>`: the client process that holds `marion-tui`'s pieces together and
/// gives that crate its first reverse dependency.
pub mod acp_child;
pub mod attach;
pub mod background;
pub mod bridge;
/// Wall-clock and entropy for minting ids: a leaf both `run` and `journal` stand on.
pub(crate) mod clock;
/// §11 item 28 step 5: the per-child MCP bridge as a **socket client**. It dials §2's socket,
/// sends `agent/spawn`, and reads the node's own stream back — carrying a request and an answer
/// rather than owning a process.
pub mod courier;
pub(crate) mod depth;
/// §7.6's descendant-gated completion: a child's `Exited` is held while a descendant is live,
/// unless it reported early or its own bound expires.
pub mod descendant_gate;
/// §5.7's start and S15's detach: how a `marion-supervisor` comes to exist detached, and how a
/// client that finds nothing listening asks for one without becoming a second start race.
pub mod detach;
pub mod doctor;
/// §6.1 step 8's readiness gate and the `stream-json` conversation behind it — **shared by the
/// root and by a child**, for the same reason the two binaries above share `run_spawn`.
pub mod duplex;
/// `events.jsonl`, writer and reader side: one node's stream on disk, and §7.3.3's one cursor.
pub mod events;
/// First-token native-facade routing, kept ahead of the legacy UTF-8 command parser.
pub mod facade_cli;
/// The seam between the registry and the socket: §2's `node/get` and `tree/subscribe`, and the
/// projection of a replayed node into something a client can be told.
pub mod handler;
/// §4.3's append-only registry journal, writer side. The records and the replay are
/// `marion-core`'s — this crate is where I/O is allowed.
pub mod journal;
/// **marion's MCP surface**, shared by `marion-supervisor mcp` (the per-child bridge) and
/// `marion mcp` (the top-level entry point). One dispatch, so there is one spawn path, and it
/// goes over the socket.
pub mod mcp;
/// Pure binding of byte-exact V2 launch state to one ready native facade descriptor.
pub mod native_binding;
/// Native-only fd bootstrap authentication and direct-CLI capability issuance.
pub mod native_bootstrap;
/// Supervisor-owned execution of an authenticated, assembled native command.
pub(crate) mod native_exec;
/// Closed native-versus-structured launch provenance and descriptor-owned lane selection.
pub mod native_intent;
/// Composition of authenticated selection, reservation, PTY launch, and claim, installed by the
/// detached supervisor through `serve::Server::start_with_native_launch`.
pub(crate) mod native_launch;
/// Transparent pane-v1 relay for an authenticated native facade claim.
#[cfg(all(
    target_has_atomic = "32",
    any(
        all(
            target_os = "macos",
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        all(
            target_os = "linux",
            any(target_arch = "aarch64", target_arch = "x86_64")
        )
    )
))]
pub(crate) mod native_relay;
mod native_tty;
mod pane_client;
/// §4.3's registry, running: `marion_core::registry::replay` as a boot path plus a tail, rather
/// than a pure function only tests call.
pub mod procid;
/// §5.3's display plane: the pty master, its recording, and the thread that reads it. The master
/// lives here and not in a client, because a client that held it would SIGHUP the agent by dying.
pub mod pty;
pub mod registry;
/// §7.2's supervisor-restart marking: the `Live` → `Orphaned` judgement applied to a replayed
/// tree, and what it refuses to decide without a process to look at.
pub mod restart;
pub mod root;
pub mod run;
/// The serve loop over §2's socket: NDJSON framing, dispatch, and §7.3.1's rule that a dropped
/// socket is not a quit.
pub mod serve;
/// The one producer of `SessionObserved`: a node's harness session id, read from its live stream
/// through its row's grammar and journaled on first sighting.
pub(crate) mod session_watch;
/// §2's unix socket: where it lives, and §5.7's start race that decides who binds it. The path is
/// the same one M2's detached supervisor will bind, which is what makes the split invisible (§10).
pub mod socket;
pub mod spawn;
pub(crate) mod spawn_receive_gate;
/// The SDK-neutral dispatch seam: tool name and arguments in, content blocks and an explicit
/// `isError` out, with everything marion means by a tool call on the far side of it.
pub mod tool;

/// `marion tree` — §5.6's tree pane, and the half of §9's M5 clause 3 that decides what to grey.
/// The drawing is `marion_tui::tree`; the deciding is here, through `doctor::capabilities_at`.
pub mod tree;
/// The journal's first production reader: what a person watching a run learns about its
/// children while it is still running.
pub mod watch;
