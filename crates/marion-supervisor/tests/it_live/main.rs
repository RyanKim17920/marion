//! **Every integration suite that drives a real harness binary**, linked as one test binary.
//!
//! A suite belongs here when it gates on a real harness — `marion_testsupport::on_path`,
//! `harness_available` or `common::script::require_claude_and_codex` — which is the same grep
//! `scripts/admit-harness.sh` selects its evidence runs by. Each suite is a module and its tests are
//! named `<suite>::<test>`: run one with `cargo test -p marion-supervisor --test it_live <suite>::`.
//!
//! The suites that need no harness are `tests/it_canned/`. A harness-driving suite that sets or
//! removes a process environment variable (`endpoint_matrix`, `repo_trust`, `token_off_argv`,
//! `user_agent_types`) stays a binary of its own: `set_var` races every other thread reading the
//! environment, and each of them assumes the environment is its alone.

#[path = "../common/mod.rs"]
mod common;

mod acp_child;
mod acp_root;
mod agy_live;
mod child_events;
mod child_stream;
mod client_run;
mod codex_app_server;
mod continuation;
mod cross_product;
mod depth_gate;
mod descendant_gate;
mod harness_matrix;
mod journal_wiring;
mod m4_fan_in;
mod native_facade_e2e;
mod native_facade_smoke;
mod native_facade_spawn;
mod no_git;
mod node_attach;
mod node_tmpdir;
mod os_sandbox_escape;
mod pane_attach;
mod permission_round_trip;
mod pi_rpc;
mod race;
mod restart_resume;
mod review;
mod timeout_kill;
mod top_level_contracted;
mod turn_delivery;
mod usage_record;
mod verification;
mod workflow;
mod worktree_reap;
