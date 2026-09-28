//! **Every bound a test puts around a real harness's boot, derived from the harness's row.**
//!
//! A boot is CPU-bound, so its wall time moves with whatever else the machine is doing: opencode's
//! ~3 CPU-seconds took ~45 s at a load average of 150 on 12 cores. A wall clock typed at a call
//! site is one measured on a quiet machine, and it is the bound that fails first on a busy one —
//! which is what every opencode and pi cell of `cross_product`, `depth_gate`, `pi_rpc` and
//! `timeout_kill` did under load, each passing on a rerun. So the number lives in one place, the
//! row's measured [`marion_harness::spec::Boot`], and the tests ask for it by agent type.

use std::time::Duration;

/// The wall clock `agent_type`'s harness is given to boot — the same budget the supervisor's own
/// readiness waits use ([`marion_harness::HarnessAdapter::boot`]).
pub fn budget(agent_type: &str) -> Duration {
    let t = marion_core::agent_type::builtin(agent_type)
        .unwrap_or_else(|| panic!("`{agent_type}` is a built-in agent type"));
    marion_harness::adapter::adapter_for_type(t.harness, t.acp_agent.as_deref())
        .unwrap_or_else(|e| panic!("`{agent_type}` has an adapter: {e}"))
        .boot()
        .budget()
}

/// A canned node's **whole run**: its boot, and the scripted turns after it, which together cost
/// no more CPU than the boot did (a canned turn is one small request and a tool call; the boot is
/// a runtime and its bundle). A stall detector — a run that ends reports long before it.
pub fn run(agent_type: &str) -> Duration {
    2 * budget(agent_type)
}

/// [`run`], in the whole seconds a `timeout_secs` field or a `--timeout` flag takes.
pub fn run_secs(agent_type: &str) -> u64 {
    run(agent_type).as_secs()
}
