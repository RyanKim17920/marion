//! UUIDv7 for `AgentId` and `TaskId` (§6.7), generated purely.
//!
//! Both ids are lowercase-hyphenated UUIDv7. That is not cosmetic:
//!
//! * `AgentId` is used **verbatim** as the `<agent_id>` directory component (§4.3), so the
//!   alphabet must stay filesystem-safe — hex and `-` only, no `/`, no case-folding collisions on
//!   a case-insensitive filesystem, no leading `.`.
//! * Cap rule 6 (§6.7) budgets a fixed-size stub on the fact that `TaskId` is exactly 36
//!   characters, so `contracts/<task_id>.json` has a length that does not depend on input.
//! * The 48-bit millisecond prefix makes ids sort in creation order, which is what makes a
//!   directory listing of `contracts/` readable as a timeline.
//!
//! Because `marion-core` performs no I/O, the generator takes **both** the clock reading and the
//! entropy as parameters. The caller owns the syscalls; this module owns the layout.
//!
//! Layout, RFC 9562 §5.7 — 16 bytes, big-endian throughout:
//!
//! ```text
//!  0        4        6        8       10                    16
//!  +--------+--------+--------+--------+---------------------+
//!  | unix_ts_ms (48 bits)     |ver|rand_a| var|rand_b (62b)  |
//!  +--------------------------+------+---+---+---------------+
//!  byte 6 high nibble  = 0b0111  (version 7)
//!  byte 8 high 2 bits  = 0b10    (variant, RFC 4122/9562)
//! ```

/// A canonical UUID string is exactly 36 characters: 32 hex + 4 hyphens.
pub const UUID_LEN: usize = 36;

/// Bytes of entropy a v7 generation consumes: 12 bits of `rand_a`, 6 bits into the variant byte,
/// and 56 bits of `rand_b` — 74 random bits in all, drawn from 10 supplied bytes.
pub const RAND_BYTES: usize = 10;

/// Build a UUIDv7 from a supplied clock reading and supplied entropy.
///
/// Only the low 48 bits of `unix_millis` are representable; the field saturates rather than
/// wrapping, because a wrapped timestamp would sort an id *before* everything already written and
/// silently break the ordering the whole scheme exists for. 2^48 ms is the year 10889, so this is
/// a guard, not a live case.
pub fn uuid_v7(unix_millis: u64, rand: [u8; RAND_BYTES]) -> String {
    const MAX_MS: u64 = (1 << 48) - 1;
    let ms = unix_millis.min(MAX_MS);
    let mut b = [0u8; 16];
    b[0] = (ms >> 40) as u8;
    b[1] = (ms >> 32) as u8;
    b[2] = (ms >> 24) as u8;
    b[3] = (ms >> 16) as u8;
    b[4] = (ms >> 8) as u8;
    b[5] = ms as u8;
    // version 7 in the high nibble, 12 bits of rand_a below it
    b[6] = 0x70 | (rand[0] & 0x0f);
    b[7] = rand[1];
    // variant 0b10 in the high two bits, 6 bits of rand_b below it
    b[8] = 0x80 | (rand[2] & 0x3f);
    b[9..16].copy_from_slice(&rand[3..10]);
    hyphenate(&b)
}

fn hyphenate(b: &[u8; 16]) -> String {
    let mut s = String::with_capacity(UUID_LEN);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble is < 16"));
        s.push(char::from_digit((byte & 0x0f) as u32, 16).expect("nibble is < 16"));
    }
    s
}

/// The millisecond timestamp a v7 id was minted at, or `None` if `s` is not a well-formed one.
///
/// Lets a replayer order records, and lets `marion doctor` spot an id from a foreign generator.
pub fn timestamp_millis(s: &str) -> Option<u64> {
    if !is_uuid_v7(s) {
        return None;
    }
    u64::from_str_radix(&format!("{}{}", &s[0..8], &s[9..13]), 16).ok()
}

/// Strict validation: exactly 36 characters, lowercase hex with hyphens at 8/13/18/23, version
/// nibble `7`, variant bits `10`.
///
/// Strict because this predicate is what makes the id safe to paste into a path (§4.3) and what
/// makes cap rule 6's stub a fixed size (§6.7). Uppercase is rejected rather than folded: two
/// spellings of one id would be two directories on Linux and one on macOS.
pub fn is_uuid_v7(s: &str) -> bool {
    if s.len() != UUID_LEN {
        return false;
    }
    let bytes = s.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        let want_hyphen = matches!(i, 8 | 13 | 18 | 23);
        let ok = if want_hyphen {
            *c == b'-'
        } else {
            c.is_ascii_digit() || (b'a'..=b'f').contains(c)
        };
        if !ok {
            return false;
        }
    }
    // version nibble is the first character of the third group; variant is the first of the fourth
    bytes[14] == b'7' && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_rfc_9562() {
        // All-zero entropy exposes the fixed bits with nothing else in the way: the timestamp is
        // big-endian across the first six bytes, byte 6's high nibble is `7`, byte 8's high two
        // bits are `10` (so the character reads `8` with zero entropy under it).
        let id = uuid_v7(0x0123_4567_89ab, [0; RAND_BYTES]);
        assert_eq!(id, "01234567-89ab-7000-8000-000000000000");

        // All-ones entropy exposes exactly which bits are *not* random: the version nibble stays
        // `7` and the variant nibble stays in 8..=b, while every other position saturates to `f`.
        let id = uuid_v7(0, [0xff; RAND_BYTES]);
        assert_eq!(id, "00000000-0000-7fff-bfff-ffffffffffff");
    }

    #[test]
    fn the_timestamp_is_the_leading_48_bits_and_round_trips() {
        let ms = 1_785_625_628_619u64; // 2026-08-01T23:07:08.619Z, matching encoding.rs's pin
        let id = uuid_v7(ms, [0xa5; RAND_BYTES]);
        assert_eq!(timestamp_millis(&id), Some(ms));
    }

    #[test]
    fn ids_minted_later_sort_later_as_strings() {
        // Lexicographic order over the hyphenated form must agree with time order, or a directory
        // listing of `contracts/` stops being a timeline.
        let a = uuid_v7(1_000, [0xff; RAND_BYTES]);
        let b = uuid_v7(1_001, [0x00; RAND_BYTES]);
        assert!(
            a < b,
            "{a} should sort before {b} despite its larger random tail"
        );
    }

    #[test]
    fn entropy_separates_ids_minted_in_the_same_millisecond() {
        // This is the defect being replaced: the supervisor's hand-rolled id derived its tail from
        // the pid alone, so two spawns in one millisecond from one process collided — and a
        // collision means two nodes sharing an `<agent_id>` directory.
        let a = uuid_v7(1_785_625_628_619, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let b = uuid_v7(1_785_625_628_619, [1, 2, 3, 4, 5, 6, 7, 8, 9, 11]);
        assert_ne!(a, b);
    }

    #[test]
    fn a_generated_id_is_thirty_six_chars_and_filesystem_safe() {
        let id = uuid_v7(1_785_625_628_619, [0x9c; RAND_BYTES]);
        assert_eq!(
            id.len(),
            UUID_LEN,
            "cap rule 6 budgets a fixed-length contract path"
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert!(
            !id.contains(std::path::MAIN_SEPARATOR),
            "must be a single path component"
        );
        assert!(
            !id.starts_with('.') && !id.starts_with('-'),
            "no hidden file, no argv flag"
        );
        assert_eq!(
            id,
            id.to_ascii_lowercase(),
            "case-insensitive filesystems must not alias ids"
        );
    }

    #[test]
    fn every_generated_id_validates() {
        for i in 0..256u32 {
            let id = uuid_v7(i as u64 * 7919, [i as u8; RAND_BYTES]);
            assert!(
                is_uuid_v7(&id),
                "generator produced an id its own validator rejects: {id}"
            );
        }
    }

    #[test]
    fn validation_rejects_near_misses() {
        let good = "0197f3aa-1c2d-7e00-8000-0102030405f0";
        assert!(is_uuid_v7(good));
        for (bad, why) in [
            (
                "0197F3AA-1C2D-7E00-8000-0102030405F0",
                "uppercase would alias on macOS",
            ),
            ("0197f3aa-1c2d-4e00-8000-0102030405f0", "v4 is not a v7"),
            (
                "0197f3aa-1c2d-7e00-c000-0102030405f0",
                "variant 0b11 is not RFC 9562",
            ),
            (
                "0197f3aa1c2d7e0080000102030405f0",
                "unhyphenated is the wrong length",
            ),
            ("0197f3aa-1c2d-7e00-8000-0102030405f", "35 characters"),
            ("0197f3aa-1c2d-7e00-8000-0102030405fg", "`g` is not hex"),
            (
                "0197f3aa/1c2d-7e00-8000-0102030405f0",
                "a separator must never validate",
            ),
        ] {
            assert!(!is_uuid_v7(bad), "{bad} should be rejected: {why}");
            assert_eq!(timestamp_millis(bad), None);
        }
    }

    #[test]
    fn a_timestamp_past_forty_eight_bits_saturates_rather_than_wrapping() {
        // A wrapped timestamp would sort before every id already written, silently inverting the
        // ordering the scheme exists for. Saturating is wrong too, but visibly and monotonically.
        let id = uuid_v7(u64::MAX, [0; RAND_BYTES]);
        assert_eq!(&id[..13], "ffffffff-ffff");
        assert!(is_uuid_v7(&id));
    }
}
