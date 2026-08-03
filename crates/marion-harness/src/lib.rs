//! marion-harness — adapters that compile a launch spec into argv + env.
//!
//! Control is config-time (README ground rule 1): marion owns the launch configuration and never
//! parses a pty. Every flag these adapters emit was measured against an installed binary.
//!
//! [`HarnessAdapter`] is the seam (design §5.2): one trait per harness, whose `compile` step *is*
//! the adapter contract — an agent type compiles to argv + env + config + MCP injection. The
//! per-harness free functions below remain the implementations behind it, and stay public because
//! their tests are the measurements.

pub mod adapter;
pub mod claude_code;
pub mod codex;
pub mod gemini;
pub mod invocation;
pub mod opencode;
pub mod stream;
pub mod surfaces;

pub use adapter::{
    ClaudeCodeAdapter, CodexAdapter, Extras, GeminiAdapter, HarnessAdapter, HarnessError,
    LaunchSpec, McpDeclaration, OpenCodeAdapter, SpawnCtx, adapter_for,
};
pub use claude_code::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, DEPTH_ENV, HeadlessSpec, McpEnv, READY_FILE_ENV,
    anthropic_base_url, compile_headless, mcp_config_json,
};
pub use codex::{ExecSpec, compile_exec, config_toml};
// `gemini` and `opencode` are addressed by module path rather than flattened here. Both define a
// `BridgeEnv` and an `MCP_ALIAS`, and both spell marion's tool names differently — a flattened
// `BridgeEnv` would make the harness a caller is configuring invisible at the use site, which is
// the exact confusion §3.1's per-harness-spelling rule exists to prevent.
pub use invocation::Invocation;
pub use stream::{ChildExit, FrameSplitter, StreamOutcome, json_frames};
pub use surfaces::{
    ControlTransport, DisplaySurface, ExecutionSurfaces, ObservationSource, TypedKind,
};
