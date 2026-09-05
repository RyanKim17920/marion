//! **The normalization §9 prescribes before a returned contract is compared to its persisted copy.**
//!
//! Shared by `m1_hop.rs`, `m4_fan_in.rs` and `cross_product.rs`, which each used to carry a copy.

use serde_json::Value;

/// Drop everything §6.7's cap rules 0–6 may shorten, **and the metadata that records the
/// shortening**, from a contract's JSON.
///
/// The returned copy is legitimately *shorter* than the persisted one — that is what the caps are
/// for — so the comparison cannot be byte equality, and §9 says exactly which fields it normalizes:
/// *"every `Capped.truncated`/`original_bytes` pair and every `*_omitted` counter may differ
/// between the two copies […] The comparison normalizes both the shortened fields and their
/// metadata; it is not an equality over the metadata."* Everything left — the ids, the repo, the
/// workspace, the scope lists, the status, the flags, the exit, the timestamps — must match byte
/// for byte.
pub fn normalize_for_cap_rules(mut v: Value) -> Value {
    // Rule 5(e).
    v["instructions"] = Value::Null;
    v["acceptance_criteria"] = Value::Null;
    if let Some(c) = v.get_mut("completion").and_then(Value::as_object_mut) {
        for key in [
            // Rules 0, 2/3, 1, 5(a)-(d).
            "narrative",
            "diff",
            "evidence",
            "changed_paths",
            "scope_violations",
            // The metadata recording the shortening.
            "evidence_omitted",
            "changed_paths_omitted",
            "scope_violations_omitted",
            "acceptance_criteria_omitted",
        ] {
            c.insert(key.to_string(), Value::Null);
        }
    }
    v
}
