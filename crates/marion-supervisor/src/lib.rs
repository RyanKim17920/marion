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
/// §4.3's append-only registry journal, writer side. The records and the replay are
/// `marion-core`'s — this crate is where I/O is allowed.
pub mod journal;
pub mod root;
pub mod run;
pub mod spawn;
