//! §6.7's cap rules 0–6, applied to the **returned** copy of a contract only.
//!
//! The persisted `contracts/<task_id>.json` is always complete; this shortens what rides back
//! through the harness, because Claude Code 2.1.220 replaces an MCP tool result over ~64–100 KB
//! with a `<persisted-output>` stub and the contract would never reach the model at all.
//!
//! Rules 0–4 are a cheap raw-byte pre-trim. **The encoded-size guarantee rests entirely on rules
//! 5 and 6**: JSON escaping can expand control-heavy output several-fold, so no raw-byte budget
//! can bound the encoded document on its own, and rule 6 is a terminal whose size does not depend
//! on the input at all.
//!
//! **Rule 5(d) and rule 6 both cover `result_commits`.** It is the only unbounded field a *child*
//! fills rather than marion, and it was hardcoded empty until commits were threaded through — so
//! rule 6's "size does not depend on the input" was true by accident rather than by construction.
//! It is now true by construction.

use crate::contract::{Capped, Completion, TaskContract};

pub const NARRATIVE_CAP: usize = 8 * 1024;
pub const DIFF_CAP: usize = 16 * 1024;
pub const EVIDENCE_BUDGET: usize = 16 * 1024;
pub const MAX_EVIDENCE: usize = 16;
pub const MAX_CRITERIA: usize = 32;
pub const PATH_CAP: usize = 512;
pub const MAX_PATHS: usize = 100;
/// Collection cap on `result_commits`, the one unbounded field a **child** fills.
///
/// Separate from [`MAX_PATHS`] despite the equal value: an object name is not a path, and a future
/// reason to move one must not silently move the other. 100 oids is ~4 KB encoded, which the
/// backstop can absorb; the field is uncapped nowhere, because rule 6 promises a terminal size
/// independent of the input and a child-filled list is exactly what could break that.
pub const MAX_COMMITS: usize = 100;
pub const BACKSTOP: usize = 48 * 1024;

/// Largest whole-character prefix of `s` fitting `max` bytes.
fn head(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// Largest whole-character suffix of `s` fitting `max` bytes.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = s.len() - max;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

fn cap_tail(c: &mut Capped<String>, max: usize) {
    if c.value.len() > max {
        c.value = tail(&c.value, max).to_string();
        c.truncated = true;
    }
}

fn cap_head(c: &mut Capped<String>, max: usize) {
    if c.value.len() > max {
        c.value = head(&c.value, max).to_string();
        c.truncated = true;
    }
}

/// Rule 5(d): a path over `PATH_CAP` becomes leading ≤255 B + `…` + trailing ≤254 B.
///
/// Leading *and* trailing, not trailing alone: `scope_violations` is judged against globs anchored
/// at the repo root, so the prefix is exactly what shows a path to be out of scope.
fn shorten_path(p: &str) -> String {
    if p.len() <= PATH_CAP {
        return p.to_string();
    }
    format!("{}…{}", head(p, 255), tail(p, 254))
}

fn encoded_len(c: &TaskContract) -> usize {
    serde_json::to_vec(c).map(|v| v.len()).unwrap_or(usize::MAX)
}

/// Rule 5's steps, in the spec's (a)–(e) order.
///
/// A table rather than a straight line so the "re-serialize after each, stop as soon as it fits"
/// loop is written once: with the check inlined between steps it was repeated five times, and a
/// sixth step would have had to remember to repeat it again.
const BACKSTOP_STEPS: [fn(&mut TaskContract); 5] = [
    rule_5a_empty_diff,
    rule_5b_drop_evidence,
    rule_5c_narrative,
    rule_5d_paths,
    rule_5e_instructions_and_criteria,
];

/// Apply the cap. Returns the copy to hand back through the harness.
pub fn cap_for_return(mut c: TaskContract) -> TaskContract {
    let Some(comp) = c.completion.as_mut() else {
        // Nothing to cap in a live or unobserved run; the struct's other fields are bounded.
        return c;
    };

    // Rules 0–3, the raw-byte pre-trim. Rule 4 (flags) is not a step of its own: every cap here
    // goes through `cap_tail`/`cap_head`, which set `truncated` as they shorten.
    rule_0_narrative(comp);
    rule_1_evidence_count(comp);
    rules_2_3_text_budget(comp);

    backstop(c)
}

/// Rule 0 — narrative, the only field a *foreign agent* writes.
fn rule_0_narrative(comp: &mut Completion) {
    if let Some(n) = comp.narrative.as_mut() {
        cap_tail(n, NARRATIVE_CAP);
    }
}

/// Rule 1 — collection cap on evidence, in `verification` order (the parent's own priority).
fn rule_1_evidence_count(comp: &mut Completion) {
    if comp.evidence.len() > MAX_EVIDENCE {
        comp.evidence_omitted += comp.evidence.len() - MAX_EVIDENCE;
        comp.evidence.truncate(MAX_EVIDENCE);
    }
}

/// Rules 2/3 — text budget and direction. Per-outcome share, halved per stream; an odd byte is
/// unused rather than handed to one stream, which two implementations would otherwise guess at.
fn rules_2_3_text_budget(comp: &mut Completion) {
    let n = comp.evidence.len();
    if n > 0 {
        let per = EVIDENCE_BUDGET / n;
        let per_stream = per / 2;
        for o in comp.evidence.iter_mut() {
            cap_tail(&mut o.stdout, per_stream);
            cap_tail(&mut o.stderr, per_stream);
        }
    }
    // diff keeps its *leading* bytes: a unified diff is only parseable from the start.
    if let Some(d) = comp.diff.as_mut() {
        cap_head(d, DIFF_CAP);
    }
}

/// Rules 5 and 6 — backstop, measured on the encoded document, applied in order, stopping as soon
/// as it fits; rule 6's stub when (a)–(e) are exhausted.
fn backstop(mut c: TaskContract) -> TaskContract {
    for step in BACKSTOP_STEPS {
        if encoded_len(&c) <= BACKSTOP {
            return c;
        }
        step(&mut c);
    }
    if encoded_len(&c) <= BACKSTOP {
        return c;
    }
    // Rule 6 — terminal. Field set is fixed and small, so its size does not depend on the input;
    // `TaskId` is a 36-character UUIDv7, so the path length is fixed and needs no escaping.
    stub(c)
}

/// Rule 5 reaches every step only with a completion, which `cap_for_return` established.
fn completion(c: &mut TaskContract) -> &mut Completion {
    c.completion.as_mut().expect("checked by cap_for_return")
}

/// Rule 5(a) — empty the diff, keeping its flag and original length.
fn rule_5a_empty_diff(c: &mut TaskContract) {
    if let Some(d) = completion(c).diff.as_mut() {
        d.value.clear();
        d.truncated = true;
    }
}

/// Rule 5(b) — drop every outcome.
fn rule_5b_drop_evidence(c: &mut TaskContract) {
    let comp = completion(c);
    comp.evidence_omitted += comp.evidence.len();
    comp.evidence.clear();
}

/// Rule 5(c) — narrative to 1 KiB.
fn rule_5c_narrative(c: &mut TaskContract) {
    if let Some(nar) = completion(c).narrative.as_mut() {
        cap_tail(nar, 1024);
    }
}

/// Rule 5(d) — elide path lists, and shorten each retained path.
fn rule_5d_paths(c: &mut TaskContract) {
    let comp = completion(c);
    if comp.changed_paths.len() > MAX_PATHS {
        comp.changed_paths_omitted += comp.changed_paths.len() - MAX_PATHS;
        comp.changed_paths.truncate(MAX_PATHS);
    }
    if comp.scope_violations.len() > MAX_PATHS {
        comp.scope_violations_omitted += comp.scope_violations.len() - MAX_PATHS;
        comp.scope_violations.truncate(MAX_PATHS);
    }
    // The child's own list, elided by the same rule and for the same reason. Unlike the two above
    // it is filled by a *foreign agent*, so it is the one whose length marion never chose.
    if comp.result_commits.len() > MAX_COMMITS {
        comp.result_commits_omitted += comp.result_commits.len() - MAX_COMMITS;
        comp.result_commits.truncate(MAX_COMMITS);
    }
    for p in comp
        .changed_paths
        .iter_mut()
        .chain(comp.scope_violations.iter_mut())
    {
        *p = shorten_path(&p.to_string_lossy()).into();
    }
}

/// Rule 5(e) — instructions and criteria.
fn rule_5e_instructions_and_criteria(c: &mut TaskContract) {
    cap_tail(&mut c.instructions, 2048);
    if c.acceptance_criteria.len() > MAX_CRITERIA {
        let dropped = c.acceptance_criteria.len() - MAX_CRITERIA;
        c.acceptance_criteria.truncate(MAX_CRITERIA);
        completion(c).acceptance_criteria_omitted += dropped;
    }
    for cr in c.acceptance_criteria.iter_mut() {
        cap_tail(cr, 2048);
    }
}

/// Rule 6's stub completion: every text field empty, every counter raised to the full dropped
/// count, and the persisted path so a reader goes there instead.
fn stub(mut c: TaskContract) -> TaskContract {
    let comp = c
        .completion
        .as_mut()
        .expect("stub is only reached with a completion");
    let dropped_paths = comp.changed_paths.len();
    let dropped_viol = comp.scope_violations.len();
    let dropped_ev = comp.evidence.len();
    let dropped_commits = comp.result_commits.len();
    comp.changed_paths_omitted += dropped_paths;
    comp.scope_violations_omitted += dropped_viol;
    comp.evidence_omitted += dropped_ev;
    comp.result_commits_omitted += dropped_commits;
    comp.changed_paths.clear();
    comp.scope_violations.clear();
    comp.evidence.clear();
    // **Without this line rule 6 is not terminal.** Its whole claim is a size that does not depend
    // on the input, and `result_commits` is filled by the child: leaving it here makes the last
    // resort O(n) in whatever a foreign agent sent, so a large enough list defeats every rule and
    // the contract goes back over the threshold that replaces it with a `<persisted-output>` stub.
    comp.result_commits.clear();
    if let Some(n) = comp.narrative.as_mut() {
        n.value.clear();
        n.truncated = true;
    }
    if let Some(d) = comp.diff.as_mut() {
        d.value.clear();
        d.truncated = true;
    }
    c.instructions.value.clear();
    c.instructions.truncated = true;
    let dropped_criteria = c.acceptance_criteria.len();
    c.acceptance_criteria.clear();
    c.completion
        .as_mut()
        .expect("checked")
        .acceptance_criteria_omitted += dropped_criteria;
    c
}
