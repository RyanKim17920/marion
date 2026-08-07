//! Reading `pty.cast` — asciicast v3, as [`CastWriter`] writes it.
//!
//! [`CastWriter`]: https://docs.rs/marion-supervisor
//!
//! The only thing this module exists to guarantee is that a **cut lands on a record boundary**.
//! `pty.cast` is JSONL and each `o` record is exactly one `read()` from the master, so the file
//! already carries the boundaries; a byte-offset cut would have to rediscover them, and would get
//! it wrong in the one case that matters. An escape sequence can straddle two `read()`s — that is
//! S11's *"a `read()` is not a frame"* MUST, and `marion-term`'s parser is streaming precisely
//! because of it — but it can **never** straddle two *records* in a way a record-index cut can
//! split, because a record-index cut keeps every record whole. A byte cut at offset `n` inside
//! `ESC [ ? 1 0 4 9 h` hands the emulator `049h`, which parses as three printable characters and
//! silently paints them.
//!
//! So the cut is an **index into `records`**, never a byte offset, and there is a test that fails
//! if someone changes that.

use std::time::Duration;

/// One asciicast v3 event. The four codes `CastWriter` emits, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// `o` — bytes the harness wrote to the master.
    Output(String),
    /// `i` — keystrokes marion injected.
    Input(String),
    /// `r` — a resize, `"COLSxROWS"`.
    Resize { cols: u16, rows: u16 },
    /// `x` — the exit status, as text.
    Exit(String),
}

/// One record, with the **absolute** offset from the session origin.
///
/// The file stores a *relative* interval per record; this module accumulates it once at parse
/// time, because every question a replay asks ("what happened in the last 120 s") is about
/// absolute time and re-accumulating per query is how off-by-one drift gets in.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub at: Duration,
    pub payload: Payload,
}

/// A parsed `pty.cast`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cast {
    /// The size from the header — the size the session *started* at, not its current one.
    pub cols: u16,
    pub rows: u16,
    pub records: Vec<Record>,
}

/// What went wrong reading a cast. A `pty.cast` marion wrote is always well-formed; a `pty.cast`
/// truncated by a SIGKILL mid-write is not, and that is the case worth naming rather than
/// panicking on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastError {
    /// The file has no header line at all.
    NoHeader,
    /// The header parsed as JSON but carried no `term.cols` / `term.rows`.
    HeaderWithoutSize,
}

impl std::fmt::Display for CastError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHeader => f.write_str("pty.cast has no header line"),
            Self::HeaderWithoutSize => f.write_str("pty.cast header carries no term.cols/rows"),
        }
    }
}

impl std::error::Error for CastError {}

impl Cast {
    /// Parse a whole `pty.cast`.
    ///
    /// **A trailing partial line is dropped, not an error.** The supervisor appends to this file
    /// while the client reads it, so reading it at any instant can catch a half-written record;
    /// that is the normal case, not corruption. Dropping it costs one record of replay, and the
    /// live `node/pty` stream carries the same bytes anyway.
    pub fn parse(text: &str) -> Result<Self, CastError> {
        let mut lines = text.lines();
        // The header must be a JSON *object*. A `.cast` whose first line is a record array is a
        // file whose header was lost, not a file with a peculiar header — and indexing an array
        // with `["term"]` yields null, which would report the wrong error.
        let header: serde_json::Value = lines
            .next()
            .and_then(|l| serde_json::from_str(l).ok())
            .filter(serde_json::Value::is_object)
            .ok_or(CastError::NoHeader)?;
        let cols = header["term"]["cols"]
            .as_u64()
            .ok_or(CastError::HeaderWithoutSize)? as u16;
        let rows = header["term"]["rows"]
            .as_u64()
            .ok_or(CastError::HeaderWithoutSize)? as u16;

        let mut records = Vec::new();
        let mut at = Duration::ZERO;
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            // A half-written trailing record parses as an error; stop, keep what is whole.
            let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
                break;
            };
            let (Some(interval), Some(code), Some(data)) =
                (ev[0].as_f64(), ev[1].as_str(), ev[2].as_str())
            else {
                break;
            };
            at += Duration::from_secs_f64(interval.max(0.0));
            let payload = match code {
                "o" => Payload::Output(data.to_owned()),
                "i" => Payload::Input(data.to_owned()),
                "r" => match parse_size(data) {
                    Some((cols, rows)) => Payload::Resize { cols, rows },
                    None => continue,
                },
                "x" => Payload::Exit(data.to_owned()),
                _ => continue,
            };
            records.push(Record { at, payload });
        }
        Ok(Self {
            cols,
            rows,
            records,
        })
    }

    /// Total elapsed time of the recording.
    pub fn duration(&self) -> Duration {
        self.records.last().map_or(Duration::ZERO, |r| r.at)
    }

    /// Bytes of `o` payload in `records[from..]`.
    pub fn output_bytes_from(&self, from: usize) -> usize {
        self.records[from.min(self.records.len())..]
            .iter()
            .filter_map(|r| match &r.payload {
                Payload::Output(s) => Some(s.len()),
                _ => None,
            })
            .sum()
    }
}

/// `"COLSxROWS"` — **columns first**, matching the cast and *not* the kernel's row-first
/// `struct winsize`. The two disagree and the only defence is to write down which this is.
fn parse_size(data: &str) -> Option<(u16, u16)> {
    let (c, r) = data.split_once('x')?;
    Some((c.trim().parse().ok()?, r.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast(body: &str) -> Cast {
        Cast::parse(&format!(
            "{{\"version\":3,\"term\":{{\"cols\":80,\"rows\":24}}}}\n{body}"
        ))
        .expect("parse")
    }

    #[test]
    fn intervals_accumulate_into_absolute_offsets() {
        let c = cast("[0.5,\"o\",\"a\"]\n[0.25,\"o\",\"b\"]\n[1.0,\"o\",\"c\"]\n");
        let at: Vec<f64> = c.records.iter().map(|r| r.at.as_secs_f64()).collect();
        assert_eq!(
            at,
            vec![0.5, 0.75, 1.75],
            "relative intervals must accumulate"
        );
    }

    #[test]
    fn the_four_codes_parse_and_unknown_ones_are_skipped() {
        let c = cast(
            "[0,\"o\",\"out\"]\n[0,\"i\",\"in\"]\n[0,\"r\",\"100x30\"]\n[0,\"m\",\"?\"]\n[0,\"x\",\"exit 0\"]\n",
        );
        assert_eq!(
            c.records
                .iter()
                .map(|r| r.payload.clone())
                .collect::<Vec<_>>(),
            vec![
                Payload::Output("out".into()),
                Payload::Input("in".into()),
                Payload::Resize {
                    cols: 100,
                    rows: 30
                },
                Payload::Exit("exit 0".into()),
            ]
        );
    }

    #[test]
    fn a_resize_is_cols_then_rows_and_not_the_kernels_row_first_order() {
        let c = cast("[0,\"r\",\"140x45\"]\n");
        assert_eq!(
            c.records[0].payload,
            Payload::Resize {
                cols: 140,
                rows: 45
            }
        );
    }

    #[test]
    fn a_half_written_trailing_record_is_dropped_rather_than_failing_the_read() {
        // The supervisor appends while the client reads. Catching a partial line is normal.
        let c = cast("[0,\"o\",\"whole\"]\n[0,\"o\",\"par");
        assert_eq!(c.records.len(), 1, "the whole record survives");
        assert_eq!(c.records[0].payload, Payload::Output("whole".into()));
    }

    #[test]
    fn a_file_with_no_header_is_an_error_and_not_an_empty_cast() {
        assert_eq!(Cast::parse(""), Err(CastError::NoHeader));
        assert_eq!(Cast::parse("[0,\"o\",\"x\"]"), Err(CastError::NoHeader));
    }

    #[test]
    fn a_header_without_a_size_is_named_rather_than_defaulted() {
        assert_eq!(
            Cast::parse("{\"version\":3}\n"),
            Err(CastError::HeaderWithoutSize),
            "defaulting to 80x24 would paint a whole session at the wrong width"
        );
    }
}
