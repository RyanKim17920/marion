//! marion-harness — adapters that compile a launch spec into argv + env.
//!
//! Control is config-time (README ground rule 1): marion owns the launch configuration and never
//! parses a pty. Every flag these adapters emit was measured against an installed binary.
//!
//! [`HarnessAdapter`] is the seam (design §5.2): one trait per harness, whose `compile` step *is*
//! the adapter contract — an agent type compiles to argv + env + config + MCP injection. The
//! per-harness free functions below remain the implementations behind it, and stay public because
//! their tests are the measurements.

pub mod acp;
pub mod adapter;
pub(crate) mod auth;
pub mod caps;
pub mod claude_code;
pub mod codex;
pub mod gemini;
pub mod invocation;
pub mod mcp_bridge;
pub mod opencode;
pub mod stream;
pub mod surfaces;

pub use acp::{AcpError, AgentHandshake};
pub use adapter::{
    AcpAdapter, ClaudeCodeAdapter, CodexAdapter, Extras, GeminiAdapter, HarnessAdapter,
    HarnessError, LaunchSpec, McpDeclaration, McpRoute, OpenCodeAdapter, SpawnCtx, adapter_for,
    adapter_for_type,
};
pub use auth::Auth;
pub use caps::{Capabilities, advertised, static_caps};
pub use claude_code::{
    HeadlessSpec, McpEnv, anthropic_base_url, compile_headless, mcp_config_json,
};
pub use codex::{ExecSpec, compile_exec, config_toml};
pub use mcp_bridge::{AGENT_ID_ENV, AGENT_TYPE_ENV, DEPTH_ENV, READY_FILE_ENV};
// `gemini` and `opencode` are addressed by module path rather than flattened here. Both define a
// `BridgeEnv` and an `MCP_ALIAS`, and both spell marion's tool names differently — a flattened
// `BridgeEnv` would make the harness a caller is configuring invisible at the use site, which is
// the exact confusion §3.1's per-harness-spelling rule exists to prevent.
pub use invocation::Invocation;
pub use stream::{CallOutcome, ChildExit, FrameSplitter, MarionCall, StreamOutcome, json_frames};
pub use surfaces::{
    ControlTransport, DisplaySurface, ExecutionSurfaces, ObservationSource, PtyWitness, TypedKind,
};
