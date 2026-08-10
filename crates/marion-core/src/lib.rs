//! marion-core — IR, launch spec, registry model, journal, task contract.
//!
//! No process spawning and no filesystem side effects, so §8's L1 tests stay pure.

pub mod agent_type;
pub mod cap;
pub mod contract;
pub mod encoding;
pub mod event;
pub mod harness;
pub mod ids;
pub mod ir;
pub mod journal;
pub mod native_facade;
pub mod node;
pub mod paths;
pub mod registry;
pub mod root_change;
pub mod scope;

pub use agent_type::{
    check_spawn_gates, default_scope_ceiling, AgentType, SpawnGateError,
    DEFAULT_MAX_CONCURRENT_CHILDREN, DEFAULT_MAX_DEPTH, DEFAULT_TIMEOUT_SECS,
};
pub use contract::{
    AgentId, Capped, ChildRef, Command, CommandOutcome, Completion, ExitStatus, Glob, Oid,
    ProcessExit, RepoIdentity, ResultStatus, TaskContract, TaskId, TaskTimestamps, Workspace,
};
pub use event::{Event, EventLog, Lifecycle, Payload, PayloadKind};
pub use harness::{Harness, UnknownHarness};
pub use ids::{new_agent_id, new_task_id, uuid_v7};
pub use ir::{Completeness, EventId, Provenance, Source, SrcSeq, Transformation};
pub use journal::{JournalRecord, RecordKind, WriterId};
pub use native_facade::{
    production_native_facades, NativeFacadeDescriptor, NativeFacadeExecutableError,
    NativeFacadeReadiness, NativeFacadeRegistry, NativeFacadeTokenError, NativeFacadeTransport,
    NativeFacadeValidationError, PRODUCTION_NATIVE_FACADES,
};
pub use node::{BlockReason, NodeState, ReapState};
pub use paths::{project_hash, state_dir, AgentDir, ProjectDir};
pub use registry::{replay, Replay, ReplayedNode, Truncation};
pub use root_change::{Reason, RootChange, RootChanged, RootDelta, RootObservation, RootScope};
