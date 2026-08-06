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

pub mod bridge;
/// §6.1 step 8's readiness gate and the `stream-json` conversation behind it — **shared by the
/// root and by a child**, for the same reason the two binaries above share `run_spawn`.
pub mod duplex;
/// `events.jsonl`, writer and reader side: one node's stream on disk, and §7.3.3's one cursor.
pub mod events;
/// The seam between the registry and the socket: §2's `node/get` and `tree/subscribe`, and the
/// projection of a replayed node into something a client can be told.
pub mod handler;
/// §4.3's append-only registry journal, writer side. The records and the replay are
/// `marion-core`'s — this crate is where I/O is allowed.
pub mod journal;
/// §4.3's registry, running: `marion_core::registry::replay` as a boot path plus a tail, rather
/// than a pure function only tests call.
pub mod registry;
pub mod root;
pub mod run;
/// The serve loop over §2's socket: NDJSON framing, dispatch, and §7.3.1's rule that a dropped
/// socket is not a quit.
pub mod serve;
/// §2's unix socket: where it lives, and §5.7's start race that decides who binds it. The path is
/// the same one M2's detached supervisor will bind, which is what makes the split invisible (§10).
pub mod socket;
pub mod spawn;
/// The journal's first production reader: what a person watching a run learns about its
/// children while it is still running.
pub mod watch;
