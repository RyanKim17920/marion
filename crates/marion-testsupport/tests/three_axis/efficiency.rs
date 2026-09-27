//! Axis 2 — efficiency.
//!
//! The supervisor runs all day; an idle marion should cost ~0 wakeups a second. The shapes that
//! break that are timer-driven waits: a `sleep` inside a loop, a `recv_timeout`/`try_wait`/
//! `wait_timeout` polled in a loop, a read timeout used as a tick, a `yield_now` spin, and the
//! named `*_POLL` period that usually drives one of them.

use syn::spanned::Spanned;

use crate::scan::{Axis, Scanner};

pub const FIX: &str = "\
Wait on the event instead of a timer: block on the fd with poll(2)/kqueue, a condvar, a channel \
`recv()` with no timeout, a pipe the other side closes, or a file-change notification. An idle \
marion should wake ~0 times a second. A bounded one-shot wait with a clear deadline (a handshake \
timeout, a kill grace period) is fine — allowlist it in checks/efficiency.allow as \
`path:item  <reason naming the deadline>`. Record measured wakeups/CPU before and after any fix.";

/// Non-blocking probes: called in a loop, the loop is a poller whatever else it does.
const PROBE_METHODS: &[&str] = &["try_recv", "try_wait"];

/// Timed waits: a poller in a loop when the timeout is a fixed period (a constant or a
/// `Duration::from_*` literal, even capped by `.min(…)`), but a bounded wait when it is the time
/// left until a deadline — the standard spurious-wakeup loop around a condvar.
const TIMED_WAIT_METHODS: &[&str] = &[
    "recv_timeout",
    "wait_timeout",
    "wait_timeout_while",
    "wait_timeout_ms",
];

/// Does this timeout expression contain a fixed period?
fn is_fixed_period(e: &syn::Expr) -> bool {
    use syn::visit::Visit;
    struct Finder(bool);
    impl<'ast> Visit<'ast> for Finder {
        fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
            let segs: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            let last = segs.last().map(String::as_str).unwrap_or("");
            let screaming = last.len() > 1
                && last
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            let duration_ctor = segs.len() >= 2 && segs[segs.len() - 2].ends_with("Duration");
            self.0 |= screaming || duration_ctor;
        }
    }
    let mut f = Finder(false);
    f.visit_expr(e);
    f.0
}

/// `DRAIN_POLL_MS`, `WATCH_TICK`: a `_`-separated word `POLL` or `TICK`. Not `POLLIN` (a poll(2)
/// flag) and not `TICKET_ATTEMPTS`.
fn names_a_period(name: &str) -> bool {
    name.split('_').any(|w| w == "POLL" || w == "TICK")
}

fn call_path(f: &syn::Expr) -> Vec<String> {
    match f {
        syn::Expr::Path(p) => p
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

pub fn check_call(s: &mut Scanner, c: &syn::ExprCall) {
    let path = call_path(&c.func);
    let Some(last) = path.last().map(String::as_str) else {
        return;
    };
    if s.loop_depth == 0 {
        return;
    }
    match last {
        "sleep" => s.emit(
            Axis::Efficiency,
            "sleep-in-loop",
            c.span(),
            format!("`{}` inside a loop is a timer-driven poll", path.join("::")),
        ),
        "yield_now" | "spin_loop" => s.emit(
            Axis::Efficiency,
            "busy-loop",
            c.span(),
            format!("`{}` inside a loop spins a core", path.join("::")),
        ),
        _ => {}
    }
}

pub fn check_method(s: &mut Scanner, m: &syn::ExprMethodCall) {
    let method = m.method.to_string();
    let probe = PROBE_METHODS.contains(&method.as_str());
    let periodic_wait =
        TIMED_WAIT_METHODS.contains(&method.as_str()) && m.args.last().is_some_and(is_fixed_period);
    if s.loop_depth > 0 && (probe || periodic_wait) {
        s.emit(
            Axis::Efficiency,
            "poll-in-loop",
            m.span(),
            format!("`.{method}(…)` inside a loop polls on a period"),
        );
    }
    // A fixed read timeout is a tick; one computed from what is left of a deadline is not.
    if method == "set_read_timeout" && m.args.first().is_some_and(is_fixed_period) {
        s.emit(
            Axis::Efficiency,
            "read-timeout-tick",
            m.span(),
            "`set_read_timeout` with a fixed period turns a blocking read into a periodic wakeup"
                .to_string(),
        );
    }
}

/// A `const`/`static` whose name says it is a poll period (`*POLL*`, `*TICK*`).
pub fn check_named_period(s: &mut Scanner, ident: &syn::Ident, expr: &syn::Expr) {
    let name = ident.to_string();
    if !names_a_period(&name) {
        return;
    }
    let value = quote::quote!(#expr).to_string().replace(' ', "");
    s.emit(
        Axis::Efficiency,
        "named-poll-period",
        ident.span(),
        format!("`{name}` = `{value}` names a polling period"),
    );
}
