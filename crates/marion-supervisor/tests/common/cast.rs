//! **Reading an asciicast** — the recording a pty host writes, shared by the suites that drive a
//! real operator terminal (`pane_attach`, `native_facade_e2e`) rather than written in each.

use std::path::Path;

/// Every `(code, data)` event in the cast at `path`, header skipped; empty when the file is not
/// there yet, which is what a poll for "has the node painted" wants to hear.
pub fn cast_records(path: &Path) -> Vec<(String, String)> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    s.lines()
        .skip(1)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let a = v.as_array()?;
            Some((
                a.get(1)?.as_str()?.to_string(),
                a.get(2)?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// The concatenated data of every `code` event: `o` is what the terminal was shown, `i` is what
/// was typed into it, `r` is each geometry it was resized to.
pub fn cast_text(path: &Path, code: &str) -> String {
    cast_records(path)
        .into_iter()
        .filter(|(c, _)| c == code)
        .map(|(_, d)| d)
        .collect()
}
