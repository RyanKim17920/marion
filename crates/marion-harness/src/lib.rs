//! marion-harness — adapters that compile a launch spec into argv + env.
//!
//! Control is config-time (README ground rule 1): marion owns the launch configuration and never
//! parses a pty. Every flag these adapters emit was measured against an installed binary.

pub mod claude_code;
pub mod codex;

pub use claude_code::{HeadlessSpec, Invocation, compile_headless};
pub use codex::{ExecSpec, compile_exec, config_toml};
