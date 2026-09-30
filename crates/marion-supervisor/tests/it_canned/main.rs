//! **Every integration suite that needs no real harness binary**, linked as one test binary.
//!
//! Rust builds each top-level file under `tests/` as its own crate, so each suite was one more
//! full link of this crate and all its dependencies. Here each suite is a module and its tests are
//! named `<suite>::<test>`: select one with `cargo test -p marion-supervisor --test it_canned
//! <suite>::`.
//!
//! Suites that drive a real harness (`on_path`, `harness_available`, `require_claude_and_codex`)
//! live in `tests/it_live/`. A suite that sets or removes a process environment variable stays a
//! binary of its own: `set_var` races every other thread reading the environment, and each such
//! suite assumes the environment is its alone. `l45_tree` stays its own target because the commit
//! hook names it, and `conformance` because it is opt-in with its own module tree.

#[path = "../common/mod.rs"]
mod common;

mod attach_verb;
mod auth_mode;
mod background_spawn;
mod cli_surface;
mod concurrent_projects;
mod containment;
mod descendant_rules;
mod detached_spawn;
mod detached_supervisor;
mod home_e2e;
mod journal;
mod launch_only_root;
mod list_offline;
mod login;
mod mcp_conformance;
mod mcp_result_reader;
mod native_bootstrap;
mod native_facade_cli;
mod native_facade_gate;
mod native_injection_binding;
mod native_mode_matrix;
mod node_kill;
mod notify_cli;
mod profiles;
mod report_on_a_root;
mod root_change_record;
mod run_stream;
mod spawned_barrier;
mod top_level_mcp;
