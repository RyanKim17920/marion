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
pub mod antigravity;
pub(crate) mod auth;
pub mod authority;
pub mod caps;
pub mod claude_code;
pub mod cline;
pub mod codex;
pub mod containment;
pub mod copilot;
pub mod env_filter;
pub mod gemini;
pub mod goose;
pub mod grammar;
pub mod invocation;
pub mod jsonl_channel;
pub mod mcp_bridge;
pub mod native;
pub mod opencode;
pub mod pi;
pub mod probe;
pub mod profile;
pub mod qwen;
pub mod rpc_channel;
pub mod spec;
pub mod stream;
pub mod surfaces;

pub use acp::AcpAdapter;
pub use acp::{AcpError, AgentHandshake};
pub use adapter::{
    Extras, HarnessAdapter, HarnessError, LaunchSpec, McpDeclaration, McpRoute, SpawnCtx,
    adapter_for, adapter_for_type, writes_files,
};
pub use antigravity::AntigravityAdapter;
pub use auth::{
    Auth, Billing, auth_failure_line, failure_cause, limit_window, reported_failure_cause,
    resets_phrase,
};
pub use caps::{Capabilities, advertised, static_caps};
pub use claude_code::ClaudeCodeAdapter;
pub use claude_code::mcp_config_json;
pub use cline::ClineAdapter;
pub use codex::CodexAdapter;
pub use codex::config_toml;
pub use copilot::CopilotAdapter;
pub use gemini::GeminiAdapter;
pub use goose::GooseAdapter;
pub use mcp_bridge::{AGENT_ID_ENV, AGENT_TYPE_ENV, BridgeEnv, DEPTH_ENV, READY_FILE_ENV};
pub use native::{
    NativeDocument, NativeEnvironmentView, NativeInjection, NativeInjectionAdapter,
    NativeInjectionError, NativeInvocation, NativeNodeContext, NativeProcessBase,
    NativeTerminalGeometry, PreparedNativeLaunch, SpecNativeAdapter, assemble_native,
    native_adapter, validate_native_process_values,
};
pub use opencode::OpenCodeAdapter;
pub use pi::PiAdapter;
pub use qwen::QwenAdapter;
// `gemini` and `opencode` are addressed by module path rather than flattened here. Both define an
// `MCP_ALIAS` and both spell marion's tool names differently — a flattened emitter would make the
// harness a caller is configuring invisible at the use site, which is the exact confusion §3.1's
// per-harness-spelling rule exists to prevent.
pub use invocation::{Invocation, TMPDIR_ENV};
pub use stream::{CallOutcome, ChildExit, FrameSplitter, MarionCall, StreamOutcome, json_frames};
pub use surfaces::{
    ControlTransport, DisplaySurface, ExecutionSurfaces, ObservationSource, PtyWitness, TypedKind,
};
