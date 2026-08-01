//! marion-core — IR, launch spec, registry model, journal, task contract.
//!
//! No process spawning and no filesystem side effects, so §8's L1 tests stay pure.

pub mod cap;
pub mod contract;
pub mod encoding;
pub mod scope;

pub use contract::{
    AgentId, Capped, ChildRef, Command, CommandOutcome, Completion, ExitStatus, Glob, Oid,
    ProcessExit, RepoIdentity, ResultStatus, TaskContract, TaskId, TaskTimestamps, Workspace,
};
