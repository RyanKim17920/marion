//! Loader for the committed `tests/fixtures/s2` pty captures.
//!
//! `NOTES.txt` is emphatic that **`.raw.bin` is authoritative**: `extract.py` decoded each pty
//! read chunk independently when building the `.cast`, so multibyte glyphs straddling a chunk
//! boundary became U+FFFD in three of the five casts. The `.cast` is still the only place resize
//! events live, so this loader takes bytes from `.raw.bin` and resize *positions* from the
//! `.cast`, and reports [`Capture::cast_matches_raw`] so a capture whose two encodings disagree
//! replays whole instead of at a guessed alignment.

#![allow(dead_code)]

use std::path::PathBuf;

pub const CLAUDE_BOOT_EXIT: &str = "claude-2.1.220-boot-exit";
pub const CLAUDE_BOOT_HELP_STATUS_RESIZE: &str = "claude-2.1.220-boot-help-status-resize";
pub const CODEX_145_DIFF_RESIZE: &str = "codex-cli-0.145.0-boot-status-help-diff-resize";
pub const CODEX_146_14ROW: &str = "codex-cli-0.146.0-14row-heavy-history-insert";
pub const CODEX_146_RESIZE: &str = "codex-cli-0.146.0-boot-status-help-resize";

pub const ALL: &[&str] = &[
    CLAUDE_BOOT_EXIT,
    CLAUDE_BOOT_HELP_STATUS_RESIZE,
    CODEX_145_DIFF_RESIZE,
    CODEX_146_14ROW,
    CODEX_146_RESIZE,
];

/// A slice of output bytes, and the resize (if any) that follows it.
pub type Segment<'a> = (&'a [u8], Option<(usize, usize)>);

/// One `raw.bin` split at the byte offsets where the pty was resized.
pub struct Capture {
    pub name: String,
    /// Initial pty size, from the asciicast header (**not** `extract.py`'s hardcoded 120x40 —
    /// `NOTES.txt` caveat (b): the 14-row capture's header correctly reads 100x14).
    pub cols: usize,
    pub rows: usize,
    /// Every output byte, in order.
    pub raw: Vec<u8>,
    /// `(byte_offset_into_raw, cols, rows)`, in order. Empty when `cast_matches_raw` is false.
    pub resizes: Vec<(usize, usize, usize)>,
    /// Whether the cast's `o` payloads concatenate to exactly `raw`.
    pub cast_matches_raw: bool,
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/s2")
}

pub fn load(name: &str) -> Capture {
    let raw = std::fs::read(dir().join(format!("{name}.raw.bin"))).expect("raw.bin");
    let cast = std::fs::read_to_string(dir().join(format!("{name}.cast"))).expect("cast");
    let mut lines = cast.lines();

    let header: serde_json::Value = serde_json::from_str(lines.next().expect("header")).unwrap();
    let cols = header["term"]["cols"].as_u64().expect("cols") as usize;
    let rows = header["term"]["rows"].as_u64().expect("rows") as usize;

    let mut resizes = Vec::new();
    let mut offset = 0usize;
    for line in lines.filter(|l| !l.trim().is_empty()) {
        let ev: serde_json::Value = serde_json::from_str(line).unwrap();
        let data = ev[2].as_str().unwrap();
        match ev[1].as_str().unwrap() {
            "o" => offset += data.len(),
            "r" => {
                let (c, r) = data.split_once('x').expect("COLSxROWS");
                resizes.push((offset, c.parse().unwrap(), r.parse().unwrap()));
            }
            _ => {}
        }
    }

    // A resize offset is a *cast* offset, so it is only usable when the cast's output bytes are
    // byte-identical to raw.bin. `NOTES.txt` says that holds for the two 0.146.0 captures and not
    // for the other three (9 damaged regions, 23 U+FFFD, from `extract.py` decoding each pty read
    // chunk independently). The damage is not invertible: Python's `decode(errors="replace")` and
    // Rust's `from_utf8_lossy` do not agree on how many U+FFFD an invalid run produces, so a
    // search for the pre-damage chunk length does not converge. A damaged capture therefore
    // replays whole at its initial size, and says so rather than replaying a guessed alignment.
    let cast_matches_raw = offset == raw.len();
    if !cast_matches_raw {
        resizes.clear();
    }

    Capture {
        name: name.to_owned(),
        cols,
        rows,
        raw,
        resizes,
        cast_matches_raw,
    }
}

impl Capture {
    /// The concatenated cast `o` payloads, for the byte-identity check `NOTES.txt` claims for the
    /// two 0.146.0 captures.
    pub fn cast_output_bytes(&self) -> Vec<u8> {
        let cast =
            std::fs::read_to_string(dir().join(format!("{}.cast", self.name))).expect("cast");
        let mut out = Vec::new();
        for line in cast.lines().skip(1).filter(|l| !l.trim().is_empty()) {
            let ev: serde_json::Value = serde_json::from_str(line).unwrap();
            if ev[1].as_str().unwrap() == "o" {
                out.extend_from_slice(ev[2].as_str().unwrap().as_bytes());
            }
        }
        out
    }

    /// Slices of `raw` between resizes, paired with the resize that follows each slice.
    pub fn segments(&self) -> Vec<Segment<'_>> {
        let mut out = Vec::new();
        let mut start = 0;
        for &(offset, c, r) in &self.resizes {
            out.push((&self.raw[start..offset], Some((c, r))));
            start = offset;
        }
        out.push((&self.raw[start..], None));
        out
    }
}
