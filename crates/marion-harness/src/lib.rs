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
pub mod copilot;
pub mod gemini;
pub mod goose;
pub mod grammar;
pub mod invocation;
pub mod mcp_bridge;
pub mod native;
pub mod opencode;
pub mod spec;
pub mod stream;
pub mod surfaces;

pub use acp::{AcpError, AgentHandshake};
pub use adapter::{
    AcpAdapter, ClaudeCodeAdapter, CodexAdapter, CopilotAdapter, Extras, GeminiAdapter,
    GooseAdapter, HarnessAdapter, HarnessError, LaunchSpec, McpDeclaration, McpRoute,
    OpenCodeAdapter, SpawnCtx, adapter_for, adapter_for_type,
};
pub use auth::Auth;
pub use caps::{Capabilities, advertised, static_caps};
pub use claude_code::{anthropic_base_url, mcp_config_json};
pub use codex::config_toml;
pub use mcp_bridge::{AGENT_ID_ENV, AGENT_TYPE_ENV, BridgeEnv, DEPTH_ENV, READY_FILE_ENV};
pub use native::{
    NativeDocument, NativeEnvironmentView, NativeInjection, NativeInjectionAdapter,
    NativeInjectionError, NativeInvocation, NativeNodeContext, NativeProcessBase,
    NativeTerminalGeometry, PreparedNativeLaunch, SpecNativeAdapter, assemble_native,
    native_adapter, validate_native_process_values,
};
// `gemini` and `opencode` are addressed by module path rather than flattened here. Both define an
// `MCP_ALIAS` and both spell marion's tool names differently — a flattened emitter would make the
// harness a caller is configuring invisible at the use site, which is the exact confusion §3.1's
// per-harness-spelling rule exists to prevent.
pub use invocation::Invocation;
pub use stream::{CallOutcome, ChildExit, FrameSplitter, MarionCall, StreamOutcome, json_frames};
pub use surfaces::{
    ControlTransport, DisplaySurface, ExecutionSurfaces, ObservationSource, PtyWitness, TypedKind,
};
