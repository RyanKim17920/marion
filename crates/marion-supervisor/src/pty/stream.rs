//! Durable binary PTY stream primitives.

// This increment deliberately lands the format and durability machinery before its production
// callsite, so every item in this module is expected to remain dark until that integration lands.
#![allow(dead_code)]

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use thiserror::Error;

const MAGIC: [u8; 8] = *b"MRNPTY01";
const VERSION: u16 = 1;
const HEADER_LEN: usize = MAGIC.len() + size_of::<u16>();
const FRAME_LEN_BYTES: usize = size_of::<u32>();
const CHECKSUM_BYTES: usize = 32;
const FRAME_FIXED_BYTES: usize = 1 + (3 * size_of::<u64>()) + size_of::<u32>();
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_RECOVERY_BYTES: usize = 16 * 1024 * 1024;
const MAX_SEGMENTS: usize = 1024;
const TRAILER_LEN: usize = MAGIC.len() + size_of::<u16>() + (4 * size_of::<u64>()) + CHECKSUM_BYTES;
const TRAILER_VERSION_START: usize = MAGIC.len();
const TRAILER_COMMITTED_LEN_START: usize = TRAILER_VERSION_START + size_of::<u16>();
const TRAILER_RECORD_SEQ_START: usize = TRAILER_COMMITTED_LEN_START + size_of::<u64>();
const TRAILER_DISPLAY_SEQ_START: usize = TRAILER_RECORD_SEQ_START + size_of::<u64>();
const TRAILER_INPUT_SEQ_START: usize = TRAILER_DISPLAY_SEQ_START + size_of::<u64>();
const TRAILER_DIGEST_START: usize = TRAILER_INPUT_SEQ_START + size_of::<u64>();

const OUTPUT_KIND: u8 = 1;
const RESIZE_KIND: u8 = 2;
const INPUT_EVIDENCE_KIND: u8 = 3;
const END_KIND: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) version: u16,
}

impl Header {
    pub(crate) fn encode(self) -> [u8; HEADER_LEN] {
        let mut encoded = [0_u8; HEADER_LEN];
        encoded[..MAGIC.len()].copy_from_slice(&MAGIC);
        encoded[MAGIC.len()..].copy_from_slice(&self.version.to_le_bytes());
        encoded
    }

    pub(crate) fn decode(encoded: &[u8]) -> Result<Self, StreamError> {
        if encoded.len() != HEADER_LEN {
            return Err(StreamError::InvalidHeaderLength {
                actual: encoded.len(),
                expected: HEADER_LEN,
            });
        }
        if encoded[..MAGIC.len()] != MAGIC {
            return Err(StreamError::BadMagic);
        }

        let version = u16::from_le_bytes(
            encoded[MAGIC.len()..]
                .try_into()
                .expect("the header length was checked"),
        );
        if version != VERSION {
            return Err(StreamError::UnsupportedVersion(version));
        }
        Ok(Self { version })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum StreamError {
    #[error("PTY stream storage error: {0}")]
    Storage(String),
    #[error("invalid PTY stream header length: got {actual}, expected {expected}")]
    InvalidHeaderLength { actual: usize, expected: usize },
    #[error("invalid PTY stream magic")]
    BadMagic,
    #[error("unsupported PTY stream version {0}")]
    UnsupportedVersion(u16),
    #[error("truncated PTY stream frame")]
    TruncatedFrame,
    #[error("PTY stream frame length {actual} exceeds maximum {max}")]
    RecordTooLarge { actual: usize, max: usize },
    #[error("unknown PTY stream record kind {0}")]
    UnknownRecordKind(u8),
    #[error("invalid PTY stream record payload")]
    InvalidRecordPayload,
    #[error("PTY stream record checksum mismatch")]
    ChecksumMismatch,
    #[error("input evidence sequence does not match its record sequence")]
    InputSequenceMismatch,
    #[error("invalid or missing PTY stream commit trailer")]
    InvalidTrailer,
    #[error("PTY stream commit trailer digest mismatch")]
    TrailerDigestMismatch,
    #[error("PTY stream record sequence is not dense: expected {expected}, got {actual}")]
    RecordSequenceGap { expected: u64, actual: u64 },
    #[error("PTY stream display sequence is invalid: expected {expected}, got {actual}")]
    DisplaySequenceGap { expected: u64, actual: u64 },
    #[error("PTY stream input sequence is invalid: expected {expected}, got {actual}")]
    InputSequenceGap { expected: u64, actual: u64 },
    #[error("PTY stream segment has no terminal End record")]
    MissingEnd,
    #[error("PTY stream record appears after End")]
    RecordAfterEnd,
    #[error("PTY stream segment ended without a commit trailer")]
    MissingTrailer,
    #[error("PTY stream recovery input exceeds maximum {max} bytes")]
    RecoveryTooLarge { max: usize },
    #[error("PTY stream recovery exceeds maximum {max} segments")]
    TooManySegments { max: usize },
    #[error("PTY stream committed length overflow")]
    CommittedLengthOverflow,
    #[error("PTY stream writer is poisoned after a prior durability failure")]
    WriterPoisoned,
}

impl From<io::Error> for StreamError {
    fn from(error: io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

/// A durable PTY record. Sequence fields are intentionally present on every frame so replay can
/// advance independent output, display, and input cursors without reconstructing missing state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) record_seq: u64,
    pub(crate) display_seq: u64,
    pub(crate) input_seq: u64,
    pub(crate) kind: RecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// Raw terminal bytes, deliberately not UTF-8 text.
    Output(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
    /// Evidence that input was accepted; its bytes are never persisted in this stream.
    InputEvidence {
        input_seq: u64,
        byte_len: u32,
    },
    End,
}

impl Record {
    /// `u32 frame_len` (little-endian), then record bytes, followed by a BLAKE3 checksum of those
    /// record bytes. The record itself uses only explicit little-endian integer encodings.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, StreamError> {
        let (kind, payload) = match &self.kind {
            RecordKind::Output(bytes) => (OUTPUT_KIND, bytes.clone()),
            RecordKind::Resize { rows, cols } => {
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&rows.to_le_bytes());
                payload.extend_from_slice(&cols.to_le_bytes());
                (RESIZE_KIND, payload)
            }
            RecordKind::InputEvidence {
                input_seq,
                byte_len,
            } => {
                if *input_seq != self.input_seq {
                    return Err(StreamError::InputSequenceMismatch);
                }
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&byte_len.to_le_bytes());
                (INPUT_EVIDENCE_KIND, payload)
            }
            RecordKind::End => (END_KIND, Vec::new()),
        };

        let record_len = FRAME_FIXED_BYTES + payload.len();
        if record_len > MAX_RECORD_BYTES {
            return Err(StreamError::RecordTooLarge {
                actual: record_len,
                max: MAX_RECORD_BYTES,
            });
        }
        let record_len = u32::try_from(record_len).map_err(|_| StreamError::RecordTooLarge {
            actual: usize::MAX,
            max: MAX_RECORD_BYTES,
        })?;

        let mut encoded =
            Vec::with_capacity(FRAME_LEN_BYTES + record_len as usize + CHECKSUM_BYTES);
        encoded.extend_from_slice(&record_len.to_le_bytes());
        encoded.push(kind);
        encoded.extend_from_slice(&self.record_seq.to_le_bytes());
        encoded.extend_from_slice(&self.display_seq.to_le_bytes());
        encoded.extend_from_slice(&self.input_seq.to_le_bytes());
        encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(blake3::hash(&encoded[FRAME_LEN_BYTES..]).as_bytes());
        Ok(encoded)
    }

    pub(crate) fn decode(encoded: &[u8]) -> Result<Self, StreamError> {
        if encoded.len() < FRAME_LEN_BYTES + CHECKSUM_BYTES {
            return Err(StreamError::TruncatedFrame);
        }
        let record_len =
            u32::from_le_bytes(encoded[..FRAME_LEN_BYTES].try_into().unwrap()) as usize;
        if record_len > MAX_RECORD_BYTES {
            return Err(StreamError::RecordTooLarge {
                actual: record_len,
                max: MAX_RECORD_BYTES,
            });
        }
        let expected_len = FRAME_LEN_BYTES + record_len + CHECKSUM_BYTES;
        if encoded.len() != expected_len || record_len < FRAME_FIXED_BYTES {
            return Err(StreamError::TruncatedFrame);
        }
        let record = &encoded[FRAME_LEN_BYTES..FRAME_LEN_BYTES + record_len];
        if blake3::hash(record).as_bytes() != &encoded[FRAME_LEN_BYTES + record_len..] {
            return Err(StreamError::ChecksumMismatch);
        }

        let kind = record[0];
        let record_seq = u64::from_le_bytes(record[1..9].try_into().unwrap());
        let display_seq = u64::from_le_bytes(record[9..17].try_into().unwrap());
        let input_seq = u64::from_le_bytes(record[17..25].try_into().unwrap());
        let payload_len = u32::from_le_bytes(record[25..29].try_into().unwrap()) as usize;
        if payload_len != record.len() - FRAME_FIXED_BYTES {
            return Err(StreamError::InvalidRecordPayload);
        }
        let payload = &record[FRAME_FIXED_BYTES..];
        let kind = match kind {
            OUTPUT_KIND => RecordKind::Output(payload.to_vec()),
            RESIZE_KIND => {
                if payload.len() != 4 {
                    return Err(StreamError::InvalidRecordPayload);
                }
                RecordKind::Resize {
                    rows: u16::from_le_bytes(payload[..2].try_into().unwrap()),
                    cols: u16::from_le_bytes(payload[2..].try_into().unwrap()),
                }
            }
            INPUT_EVIDENCE_KIND => RecordKind::InputEvidence {
                input_seq,
                byte_len: u32::from_le_bytes(
                    payload
                        .try_into()
                        .map_err(|_| StreamError::InvalidRecordPayload)?,
                ),
            },
            END_KIND if payload.is_empty() => RecordKind::End,
            END_KIND => return Err(StreamError::InvalidRecordPayload),
            other => return Err(StreamError::UnknownRecordKind(other)),
        };
        Ok(Self {
            record_seq,
            display_seq,
            input_seq,
            kind,
        })
    }
}

/// An in-memory, fully committed PTY recording segment. A trailer makes a segment self-validating
/// before a future writer exposes it on disk: it repeats the format identity and binds every byte
/// before it, including the header and each per-frame checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) records: Vec<Record>,
}

impl Segment {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, StreamError> {
        let counters = validate_records(&self.records)?;
        let mut encoded = Header { version: VERSION }.encode().to_vec();
        for record in &self.records {
            encoded.extend_from_slice(&record.encode()?);
        }
        encoded.extend_from_slice(&trailer_bytes(&encoded, counters));
        Ok(encoded)
    }

    pub(crate) fn decode(encoded: &[u8]) -> Result<Self, StreamError> {
        if encoded.len() < HEADER_LEN + TRAILER_LEN {
            return Err(StreamError::InvalidTrailer);
        }
        Header::decode(&encoded[..HEADER_LEN])?;
        let trailer_start = encoded.len() - TRAILER_LEN;
        let trailer = &encoded[trailer_start..];
        if trailer[..TRAILER_VERSION_START] != MAGIC
            || u16::from_le_bytes(
                trailer[TRAILER_VERSION_START..TRAILER_COMMITTED_LEN_START]
                    .try_into()
                    .unwrap(),
            ) != VERSION
        {
            return Err(StreamError::InvalidTrailer);
        }
        let committed_len = usize::try_from(u64::from_le_bytes(
            trailer[TRAILER_COMMITTED_LEN_START..TRAILER_RECORD_SEQ_START]
                .try_into()
                .unwrap(),
        ))
        .map_err(|_| StreamError::InvalidTrailer)?;
        if committed_len != trailer_start {
            return Err(StreamError::InvalidTrailer);
        }
        if blake3::hash(&encoded[..trailer_start]).as_bytes() != &trailer[TRAILER_DIGEST_START..] {
            return Err(StreamError::TrailerDigestMismatch);
        }

        let mut records = Vec::new();
        let mut offset = HEADER_LEN;
        while offset < trailer_start {
            if trailer_start - offset < FRAME_LEN_BYTES {
                return Err(StreamError::InvalidTrailer);
            }
            let frame_len = u32::from_le_bytes(
                encoded[offset..offset + FRAME_LEN_BYTES]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if !(FRAME_FIXED_BYTES..=MAX_RECORD_BYTES).contains(&frame_len) {
                return Err(StreamError::InvalidTrailer);
            }
            let frame_end = offset + FRAME_LEN_BYTES + frame_len + CHECKSUM_BYTES;
            if frame_end > trailer_start {
                return Err(StreamError::InvalidTrailer);
            }
            records.push(Record::decode(&encoded[offset..frame_end])?);
            offset = frame_end;
        }
        let counters = validate_records(&records)?;
        let terminal = TrailerCounters {
            record_seq: u64::from_le_bytes(
                trailer[TRAILER_RECORD_SEQ_START..TRAILER_DISPLAY_SEQ_START]
                    .try_into()
                    .unwrap(),
            ),
            display_seq: u64::from_le_bytes(
                trailer[TRAILER_DISPLAY_SEQ_START..TRAILER_INPUT_SEQ_START]
                    .try_into()
                    .unwrap(),
            ),
            input_seq: u64::from_le_bytes(
                trailer[TRAILER_INPUT_SEQ_START..TRAILER_DIGEST_START]
                    .try_into()
                    .unwrap(),
            ),
        };
        if terminal != counters {
            return Err(StreamError::InvalidTrailer);
        }
        Ok(Self { records })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrailerCounters {
    record_seq: u64,
    display_seq: u64,
    input_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveredSegment {
    pub(crate) end: usize,
    pub(crate) record_seq: u64,
    pub(crate) display_seq: u64,
    pub(crate) input_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recovery {
    pub(crate) segments: Vec<RecoveredSegment>,
    pub(crate) truncate_to: Option<usize>,
}

trait DurableStorage {
    fn position(&mut self, offset: u64) -> io::Result<()>;
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn sync_data(&mut self) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    fn read_all(&mut self) -> io::Result<Vec<u8>>;
}

impl DurableStorage for File {
    fn position(&mut self, offset: u64) -> io::Result<()> {
        self.seek(SeekFrom::Start(offset)).map(|_| ())
    }
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        Write::write_all(self, bytes)
    }
    fn sync_data(&mut self) -> io::Result<()> {
        File::sync_data(self)
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.read_to_end(&mut bytes)?;
        self.seek(SeekFrom::End(0))?;
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommitReceipt {
    pub(crate) committed_len: u64,
}

struct SegmentWriter<S> {
    storage: S,
    committed_len: u64,
    poisoned: bool,
}

impl<S: DurableStorage> SegmentWriter<S> {
    fn new(mut storage: S) -> Result<Self, StreamError> {
        let committed_len = storage.len()?;
        storage.position(committed_len)?;
        Ok(Self {
            committed_len,
            storage,
            poisoned: false,
        })
    }

    fn commit(&mut self, segment: &Segment) -> Result<CommitReceipt, StreamError> {
        if self.poisoned {
            return Err(StreamError::WriterPoisoned);
        }
        let bytes = segment.encode()?;
        let byte_len =
            u64::try_from(bytes.len()).map_err(|_| StreamError::CommittedLengthOverflow)?;
        let next_committed_len = self
            .committed_len
            .checked_add(byte_len)
            .ok_or(StreamError::CommittedLengthOverflow)?;
        self.storage.position(self.committed_len)?;
        if let Err(error) = self.storage.write_all(&bytes) {
            self.poisoned = true;
            return Err(error.into());
        }
        if let Err(error) = self.storage.sync_data() {
            self.poisoned = true;
            return Err(error.into());
        }
        self.committed_len = next_committed_len;
        Ok(CommitReceipt {
            committed_len: self.committed_len,
        })
    }

    fn recover_and_repair(&mut self) -> Result<Recovery, StreamError> {
        // Recovery is the only operation that can clear poison. Keep it set until every validation
        // and durability step has succeeded, so no failure path can accidentally re-enable writes.
        self.poisoned = true;
        let bytes = self.storage.read_all()?;
        let recovery = recover(&bytes)?;
        let recovered_len = match recovery.truncate_to {
            Some(truncate_to) => {
                let truncate_to =
                    u64::try_from(truncate_to).map_err(|_| StreamError::CommittedLengthOverflow)?;
                self.storage.set_len(truncate_to)?;
                truncate_to
            }
            None => u64::try_from(bytes.len()).map_err(|_| StreamError::CommittedLengthOverflow)?,
        };
        self.storage.sync_data()?;
        self.storage.position(recovered_len)?;
        self.committed_len = recovered_len;
        self.poisoned = false;
        Ok(recovery)
    }
}

/// Recover only complete, sequential segments. It deliberately never searches for a later magic
/// marker: bytes between commits are either the next segment's prefix or corruption.
pub(crate) fn recover(encoded: &[u8]) -> Result<Recovery, StreamError> {
    if encoded.len() > MAX_RECOVERY_BYTES {
        return Err(StreamError::RecoveryTooLarge {
            max: MAX_RECOVERY_BYTES,
        });
    }
    let mut offset = 0;
    let mut segments = Vec::new();
    while offset < encoded.len() {
        if segments.len() == MAX_SEGMENTS {
            return Err(StreamError::TooManySegments { max: MAX_SEGMENTS });
        }
        match scan_segment(&encoded[offset..])? {
            Scan::Complete(segment, used) => {
                let end = offset + used;
                let last = segment.records.last().expect("validated End record");
                segments.push(RecoveredSegment {
                    end,
                    record_seq: last.record_seq,
                    display_seq: last.display_seq,
                    input_seq: last.input_seq,
                });
                offset = end;
            }
            Scan::Prefix => {
                if segments.is_empty() {
                    return Err(StreamError::InvalidTrailer);
                }
                return Ok(Recovery {
                    segments,
                    truncate_to: Some(offset),
                });
            }
            Scan::MissingTrailer => {
                if segments.is_empty() {
                    return Err(StreamError::MissingTrailer);
                }
                return Ok(Recovery {
                    segments,
                    truncate_to: Some(offset),
                });
            }
        }
    }
    Ok(Recovery {
        segments,
        truncate_to: None,
    })
}

enum Scan {
    Complete(Segment, usize),
    Prefix,
    MissingTrailer,
}

fn scan_segment(bytes: &[u8]) -> Result<Scan, StreamError> {
    if bytes.len() < HEADER_LEN {
        let expected = Header { version: VERSION }.encode();
        return if bytes == &expected[..bytes.len()] {
            Ok(Scan::Prefix)
        } else {
            Err(StreamError::BadMagic)
        };
    }
    Header::decode(&bytes[..HEADER_LEN])?;
    let mut offset = HEADER_LEN;
    let mut records = Vec::new();
    loop {
        if bytes.len() - offset < FRAME_LEN_BYTES {
            return Ok(Scan::Prefix);
        }
        let body_len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        if !(FRAME_FIXED_BYTES..=MAX_RECORD_BYTES).contains(&body_len) {
            return Err(StreamError::InvalidTrailer);
        }
        let frame_end = offset + FRAME_LEN_BYTES + body_len + CHECKSUM_BYTES;
        if frame_end > bytes.len() {
            return Ok(Scan::Prefix);
        }
        let record = Record::decode(&bytes[offset..frame_end])?;
        let is_end = matches!(&record.kind, RecordKind::End);
        records.push(record);
        offset = frame_end;
        if is_end {
            break;
        }
    }
    let counters = validate_records(&records)?;
    if bytes.len() == offset {
        return Ok(Scan::MissingTrailer);
    }
    if bytes.len() - offset < TRAILER_LEN {
        let expected = trailer_bytes(&bytes[..offset], counters);
        return if bytes[offset..] == expected[..bytes.len() - offset] {
            Ok(Scan::Prefix)
        } else {
            Err(StreamError::InvalidTrailer)
        };
    }
    let used = offset + TRAILER_LEN;
    let segment = Segment::decode(&bytes[..used])?;
    Ok(Scan::Complete(segment, used))
}

fn trailer_bytes(committed: &[u8], counters: TrailerCounters) -> [u8; TRAILER_LEN] {
    let mut trailer = [0; TRAILER_LEN];
    trailer[..TRAILER_VERSION_START].copy_from_slice(&MAGIC);
    trailer[TRAILER_VERSION_START..TRAILER_COMMITTED_LEN_START]
        .copy_from_slice(&VERSION.to_le_bytes());
    trailer[TRAILER_COMMITTED_LEN_START..TRAILER_RECORD_SEQ_START]
        .copy_from_slice(&(committed.len() as u64).to_le_bytes());
    trailer[TRAILER_RECORD_SEQ_START..TRAILER_DISPLAY_SEQ_START]
        .copy_from_slice(&counters.record_seq.to_le_bytes());
    trailer[TRAILER_DISPLAY_SEQ_START..TRAILER_INPUT_SEQ_START]
        .copy_from_slice(&counters.display_seq.to_le_bytes());
    trailer[TRAILER_INPUT_SEQ_START..TRAILER_DIGEST_START]
        .copy_from_slice(&counters.input_seq.to_le_bytes());
    trailer[TRAILER_DIGEST_START..].copy_from_slice(blake3::hash(committed).as_bytes());
    trailer
}

fn validate_records(records: &[Record]) -> Result<TrailerCounters, StreamError> {
    let mut counters = TrailerCounters {
        record_seq: 0,
        display_seq: 0,
        input_seq: 0,
    };
    let mut ended = false;
    for record in records {
        if ended {
            return Err(StreamError::RecordAfterEnd);
        }
        let expected_record = counters.record_seq + 1;
        if record.record_seq != expected_record {
            return Err(StreamError::RecordSequenceGap {
                expected: expected_record,
                actual: record.record_seq,
            });
        }
        counters.record_seq = record.record_seq;

        let advances_display = matches!(
            &record.kind,
            RecordKind::Output(_) | RecordKind::Resize { .. }
        );
        let expected_display = counters.display_seq + u64::from(advances_display);
        if record.display_seq != expected_display {
            return Err(StreamError::DisplaySequenceGap {
                expected: expected_display,
                actual: record.display_seq,
            });
        }
        counters.display_seq = record.display_seq;

        let expected_input = counters.input_seq
            + u64::from(matches!(&record.kind, RecordKind::InputEvidence { .. }));
        if record.input_seq != expected_input {
            return Err(StreamError::InputSequenceGap {
                expected: expected_input,
                actual: record.input_seq,
            });
        }
        if let RecordKind::InputEvidence { input_seq, .. } = &record.kind
            && *input_seq != record.input_seq
        {
            return Err(StreamError::InputSequenceMismatch);
        }
        counters.input_seq = record.input_seq;
        ended = matches!(&record.kind, RecordKind::End);
    }
    if !ended {
        return Err(StreamError::MissingEnd);
    }
    Ok(counters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_stream_header_roundtrip() {
        let encoded = Header { version: VERSION }.encode();

        assert_eq!(&encoded[..MAGIC.len()], &MAGIC);
        assert_eq!(&encoded[MAGIC.len()..], &VERSION.to_le_bytes());
        assert_eq!(Header::decode(&encoded), Ok(Header { version: VERSION }));
    }

    #[test]
    fn pty_stream_rejects_bad_magic_and_version() {
        let mut bad_magic = Header { version: VERSION }.encode();
        bad_magic[0] ^= 1;
        assert_eq!(Header::decode(&bad_magic), Err(StreamError::BadMagic));

        let mut bad_version = Header { version: VERSION }.encode();
        bad_version[MAGIC.len()..].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(
            Header::decode(&bad_version),
            Err(StreamError::UnsupportedVersion(VERSION + 1))
        );
    }

    fn record(kind: RecordKind) -> Record {
        Record {
            record_seq: 7,
            display_seq: 11,
            input_seq: 13,
            kind,
        }
    }

    #[test]
    fn pty_stream_output_roundtrips_opaque_invalid_utf8() {
        let original = record(RecordKind::Output(vec![0xff, 0xfe, 0, b'x']));
        assert_eq!(Record::decode(&original.encode().unwrap()), Ok(original));
    }

    #[test]
    fn pty_stream_input_evidence_persists_no_input_bytes() {
        let raw_input_sentinel = b"input bytes must never be persisted";
        let original = record(RecordKind::InputEvidence {
            input_seq: 13,
            byte_len: 4,
        });
        let encoded = original.encode().unwrap();
        assert_eq!(Record::decode(&encoded), Ok(original));
        assert_eq!(
            encoded.len(),
            FRAME_LEN_BYTES + FRAME_FIXED_BYTES + 4 + CHECKSUM_BYTES
        );
        assert!(
            !encoded
                .windows(raw_input_sentinel.len())
                .any(|window| window == raw_input_sentinel),
            "input evidence must encode metadata only, never raw input bytes"
        );
    }

    #[test]
    fn pty_stream_rejects_unknown_kind_oversize_and_bad_checksum() {
        let original = record(RecordKind::End);
        let mut unknown = original.encode().unwrap();
        unknown[FRAME_LEN_BYTES] = 99;
        let checksum_start = unknown.len() - CHECKSUM_BYTES;
        let checksum = blake3::hash(&unknown[FRAME_LEN_BYTES..checksum_start]);
        unknown[checksum_start..].copy_from_slice(checksum.as_bytes());
        assert_eq!(
            Record::decode(&unknown),
            Err(StreamError::UnknownRecordKind(99))
        );

        let oversized = record(RecordKind::Output(vec![0; MAX_RECORD_BYTES]));
        assert!(matches!(
            oversized.encode(),
            Err(StreamError::RecordTooLarge { .. })
        ));

        let mut corrupted = original.encode().unwrap();
        corrupted[FRAME_LEN_BYTES] ^= 1;
        assert_eq!(
            Record::decode(&corrupted),
            Err(StreamError::ChecksumMismatch)
        );
    }

    fn mixed_segment() -> Segment {
        Segment {
            records: vec![
                Record {
                    record_seq: 1,
                    display_seq: 1,
                    input_seq: 0,
                    kind: RecordKind::Output(vec![0xff]),
                },
                Record {
                    record_seq: 2,
                    display_seq: 2,
                    input_seq: 0,
                    kind: RecordKind::Resize { rows: 24, cols: 80 },
                },
                Record {
                    record_seq: 3,
                    display_seq: 2,
                    input_seq: 1,
                    kind: RecordKind::InputEvidence {
                        input_seq: 1,
                        byte_len: 3,
                    },
                },
                Record {
                    record_seq: 4,
                    display_seq: 2,
                    input_seq: 1,
                    kind: RecordKind::End,
                },
            ],
        }
    }

    #[test]
    fn pty_stream_segment_roundtrips_mixed_records_and_terminal_counters() {
        let segment = mixed_segment();
        let encoded = segment.encode().unwrap();
        assert_eq!(
            u64::from_le_bytes(
                encoded[encoded.len() - TRAILER_LEN + 18..encoded.len() - TRAILER_LEN + 26]
                    .try_into()
                    .unwrap()
            ),
            4
        );
        assert_eq!(
            u64::from_le_bytes(
                encoded[encoded.len() - TRAILER_LEN + 26..encoded.len() - TRAILER_LEN + 34]
                    .try_into()
                    .unwrap()
            ),
            2
        );
        assert_eq!(
            u64::from_le_bytes(
                encoded[encoded.len() - TRAILER_LEN + 34..encoded.len() - TRAILER_LEN + 42]
                    .try_into()
                    .unwrap()
            ),
            1
        );
        assert_eq!(Segment::decode(&encoded), Ok(segment));
    }

    #[test]
    fn pty_stream_segment_rejects_sequence_gaps_and_missing_end() {
        let mut record_gap = mixed_segment();
        record_gap.records[1].record_seq = 3;
        assert!(matches!(
            record_gap.encode(),
            Err(StreamError::RecordSequenceGap { .. })
        ));
        let mut display_gap = mixed_segment();
        display_gap.records[1].display_seq = 3;
        assert!(matches!(
            display_gap.encode(),
            Err(StreamError::DisplaySequenceGap { .. })
        ));
        let mut input_gap = mixed_segment();
        input_gap.records[2].input_seq = 2;
        assert!(matches!(
            input_gap.encode(),
            Err(StreamError::InputSequenceGap { .. })
        ));
        let mut missing_end = mixed_segment();
        missing_end.records.pop();
        assert_eq!(missing_end.encode(), Err(StreamError::MissingEnd));
    }

    #[test]
    fn pty_stream_segment_rejects_records_after_end_and_bad_trailer_or_digest() {
        let mut after_end = mixed_segment();
        after_end.records.push(Record {
            record_seq: 5,
            display_seq: 2,
            input_seq: 1,
            kind: RecordKind::End,
        });
        assert_eq!(after_end.encode(), Err(StreamError::RecordAfterEnd));

        let encoded = mixed_segment().encode().unwrap();
        let mut missing_trailer = encoded.clone();
        missing_trailer.truncate(missing_trailer.len() - TRAILER_LEN);
        assert_eq!(
            Segment::decode(&missing_trailer),
            Err(StreamError::InvalidTrailer)
        );
        let mut reordered_trailer = encoded.clone();
        reordered_trailer[encoded.len() - TRAILER_LEN] ^= 1;
        assert_eq!(
            Segment::decode(&reordered_trailer),
            Err(StreamError::InvalidTrailer)
        );
        let mut corrupt_digest = encoded;
        let digest_start = corrupt_digest.len() - CHECKSUM_BYTES;
        corrupt_digest[digest_start] ^= 1;
        assert_eq!(
            Segment::decode(&corrupt_digest),
            Err(StreamError::TrailerDigestMismatch)
        );
    }

    #[test]
    fn pty_stream_recovery_reads_two_segments_and_truncates_valid_suffixes() {
        let first = mixed_segment().encode().unwrap();
        let second = mixed_segment().encode().unwrap();
        let mut both = first.clone();
        both.extend_from_slice(&second);
        assert_eq!(recover(&both).unwrap().segments.len(), 2);

        for suffix in [
            &second[..3],
            &second[..HEADER_LEN + 2],
            &second[..second.len() - 3],
        ] {
            let mut partial = first.clone();
            partial.extend_from_slice(suffix);
            assert_eq!(recover(&partial).unwrap().truncate_to, Some(first.len()));
        }
        assert_eq!(recover(&second[..3]), Err(StreamError::InvalidTrailer));
    }

    #[test]
    fn pty_stream_recovery_rejects_missing_or_corrupt_interior_and_bounds() {
        let complete = mixed_segment().encode().unwrap();
        assert_eq!(
            recover(&complete[..complete.len() - TRAILER_LEN]),
            Err(StreamError::MissingTrailer)
        );
        let mut corrupt = complete.clone();
        corrupt[HEADER_LEN + FRAME_LEN_BYTES] ^= 1;
        assert_eq!(recover(&corrupt), Err(StreamError::ChecksumMismatch));
        assert!(matches!(
            recover(&vec![0; MAX_RECOVERY_BYTES + 1]),
            Err(StreamError::RecoveryTooLarge { .. })
        ));
        let mut too_many = Vec::new();
        for _ in 0..=MAX_SEGMENTS {
            too_many.extend_from_slice(&complete);
        }
        assert!(matches!(
            recover(&too_many),
            Err(StreamError::TooManySegments { .. })
        ));
    }

    #[test]
    fn pty_stream_recovery_truncates_end_at_eof_only_after_a_prior_commit() {
        let complete = mixed_segment().encode().unwrap();
        let without_trailer = &complete[..complete.len() - TRAILER_LEN];

        assert_eq!(recover(without_trailer), Err(StreamError::MissingTrailer));

        let mut concatenated = complete.clone();
        concatenated.extend_from_slice(without_trailer);
        let recovery = recover(&concatenated).unwrap();
        assert_eq!(recovery.segments.len(), 1);
        assert_eq!(recovery.truncate_to, Some(complete.len()));
    }

    #[test]
    fn pty_stream_recovery_rejects_wrong_or_complete_corrupt_trailer_after_prior_commit() {
        let complete = mixed_segment().encode().unwrap();
        let record_end = complete.len() - TRAILER_LEN;

        let mut wrong_prefix = complete.clone();
        wrong_prefix.extend_from_slice(&complete[..record_end]);
        wrong_prefix.push(MAGIC[0] ^ 1);
        assert_eq!(recover(&wrong_prefix), Err(StreamError::InvalidTrailer));

        let mut corrupt_complete = complete.clone();
        corrupt_complete[record_end] ^= 1;
        let mut concatenated = complete.clone();
        concatenated.extend_from_slice(&corrupt_complete);
        assert_eq!(recover(&concatenated), Err(StreamError::InvalidTrailer));
    }

    #[derive(Default)]
    struct FakeStorage {
        bytes: Vec<u8>,
        cursor: usize,
        events: Vec<&'static str>,
        fail_write_after: Option<usize>,
        fail_sync: bool,
        fail_set_len: bool,
        fail_position: bool,
    }
    impl DurableStorage for FakeStorage {
        fn position(&mut self, offset: u64) -> io::Result<()> {
            if self.fail_position {
                return Err(io::Error::other("position"));
            }
            self.cursor = usize::try_from(offset).map_err(|_| io::Error::other("position"))?;
            Ok(())
        }
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.events.push("write");
            let bytes = if let Some(prefix_len) = self.fail_write_after {
                &bytes[..prefix_len.min(bytes.len())]
            } else {
                bytes
            };
            let end = self
                .cursor
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::other("write overflow"))?;
            self.bytes.resize(self.bytes.len().max(end), 0);
            self.bytes[self.cursor..end].copy_from_slice(bytes);
            self.cursor = end;
            if self.fail_write_after.is_some() {
                Err(io::Error::other("partial write"))
            } else {
                Ok(())
            }
        }
        fn sync_data(&mut self) -> io::Result<()> {
            self.events.push("sync");
            if self.fail_sync {
                Err(io::Error::other("sync"))
            } else {
                Ok(())
            }
        }
        fn len(&self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }
        fn set_len(&mut self, len: u64) -> io::Result<()> {
            self.events.push("set_len");
            if self.fail_set_len {
                Err(io::Error::other("set_len"))
            } else {
                self.bytes.truncate(len as usize);
                Ok(())
            }
        }
        fn read_all(&mut self) -> io::Result<Vec<u8>> {
            self.events.push("read");
            self.cursor = self.bytes.len();
            Ok(self.bytes.clone())
        }
    }

    #[test]
    fn pty_stream_writer_syncs_before_publishing_and_does_not_advance_on_failure() {
        let mut writer = SegmentWriter::new(FakeStorage::default()).unwrap();
        let receipt = writer.commit(&mixed_segment()).unwrap();
        assert_eq!(writer.storage.events, ["write", "sync"]);
        assert_eq!(receipt.committed_len, writer.committed_len);

        let mut failing = SegmentWriter::new(FakeStorage {
            fail_sync: true,
            ..FakeStorage::default()
        })
        .unwrap();
        assert!(matches!(
            failing.commit(&mixed_segment()),
            Err(StreamError::Storage(_))
        ));
        assert_eq!(failing.committed_len, 0);
        assert_eq!(failing.storage.events, ["write", "sync"]);
        assert_eq!(
            failing.commit(&mixed_segment()),
            Err(StreamError::WriterPoisoned)
        );
        assert_eq!(failing.storage.events, ["write", "sync"]);
    }

    #[test]
    fn pty_stream_writer_partial_write_poison_prevents_retry_storage_access() {
        let mut writer = SegmentWriter::new(FakeStorage {
            fail_write_after: Some(7),
            ..FakeStorage::default()
        })
        .unwrap();

        assert!(matches!(
            writer.commit(&mixed_segment()),
            Err(StreamError::Storage(_))
        ));
        assert_eq!(writer.committed_len, 0);
        assert_eq!(writer.storage.bytes.len(), 7);
        assert_eq!(writer.storage.events, ["write"]);

        assert_eq!(
            writer.commit(&mixed_segment()),
            Err(StreamError::WriterPoisoned)
        );
        assert_eq!(writer.storage.bytes.len(), 7);
        assert_eq!(writer.storage.events, ["write"]);
    }

    #[test]
    fn pty_stream_writer_successful_repair_clears_poison_and_allows_commit() {
        let complete = mixed_segment().encode().unwrap();
        let mut writer = SegmentWriter::new(FakeStorage {
            bytes: complete.clone(),
            fail_write_after: Some(7),
            ..FakeStorage::default()
        })
        .unwrap();
        assert!(matches!(
            writer.commit(&mixed_segment()),
            Err(StreamError::Storage(_))
        ));
        assert!(writer.poisoned);

        writer.storage.fail_write_after = None;
        writer.recover_and_repair().unwrap();
        assert!(!writer.poisoned);
        writer.commit(&mixed_segment()).unwrap();
        assert_eq!(recover(&writer.storage.bytes).unwrap().segments.len(), 2);
    }

    #[test]
    fn pty_stream_writer_failed_repair_stays_poisoned_before_retry() {
        let complete = mixed_segment().encode().unwrap();
        let mut torn = complete.clone();
        torn.extend_from_slice(&complete[..3]);

        let mut sync_failure = SegmentWriter::new(FakeStorage {
            bytes: torn.clone(),
            fail_sync: true,
            ..FakeStorage::default()
        })
        .unwrap();
        assert!(matches!(
            sync_failure.recover_and_repair(),
            Err(StreamError::Storage(_))
        ));
        assert!(sync_failure.poisoned);
        let events = sync_failure.storage.events.clone();
        assert_eq!(events, ["read", "set_len", "sync"]);
        assert_eq!(
            sync_failure.commit(&mixed_segment()),
            Err(StreamError::WriterPoisoned)
        );
        assert_eq!(sync_failure.storage.events, events);

        let mut position_failure = SegmentWriter::new(FakeStorage {
            bytes: torn,
            ..FakeStorage::default()
        })
        .unwrap();
        position_failure.storage.fail_position = true;
        assert!(matches!(
            position_failure.recover_and_repair(),
            Err(StreamError::Storage(_))
        ));
        assert!(position_failure.poisoned);
        let events = position_failure.storage.events.clone();
        assert_eq!(events, ["read", "set_len", "sync"]);
        assert_eq!(
            position_failure.commit(&mixed_segment()),
            Err(StreamError::WriterPoisoned)
        );
        assert_eq!(position_failure.storage.events, events);
    }

    #[test]
    fn pty_stream_writer_repairs_after_set_len_then_sync_and_real_file_is_truncated() {
        let complete = mixed_segment().encode().unwrap();
        let mut fake = FakeStorage {
            bytes: complete.clone(),
            ..FakeStorage::default()
        };
        fake.bytes.extend_from_slice(&complete[..3]);
        let mut writer = SegmentWriter::new(fake).unwrap();
        let repaired = writer.recover_and_repair().unwrap();
        assert_eq!(repaired.truncate_to, Some(complete.len()));
        assert_eq!(writer.storage.events, ["read", "set_len", "sync"]);

        let path = std::env::temp_dir().join(format!("marion-pty-stream-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        Write::write_all(&mut file, &complete).unwrap();
        Write::write_all(&mut file, &complete[..3]).unwrap();
        let mut writer = SegmentWriter::new(file).unwrap();
        writer.recover_and_repair().unwrap();
        assert_eq!(writer.storage.read_all().unwrap(), complete);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pty_stream_writer_reopen_positions_at_eof_before_commit() {
        let first = mixed_segment().encode().unwrap();
        let path =
            std::env::temp_dir().join(format!("marion-pty-stream-reopen-{}", std::process::id()));
        std::fs::write(&path, &first).unwrap();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut writer = SegmentWriter::new(file).unwrap();
        writer.commit(&mixed_segment()).unwrap();
        let bytes = writer.storage.read_all().unwrap();

        assert_eq!(&bytes[..first.len()], first);
        assert_eq!(recover(&bytes).unwrap().segments.len(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pty_stream_writer_repair_positions_at_truncation_before_next_commit() {
        let complete = mixed_segment().encode().unwrap();
        let path = std::env::temp_dir().join(format!(
            "marion-pty-stream-repair-append-{}",
            std::process::id()
        ));
        let mut torn = complete.clone();
        torn.extend_from_slice(&complete[..3]);
        std::fs::write(&path, torn).unwrap();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut writer = SegmentWriter::new(file).unwrap();
        writer.recover_and_repair().unwrap();
        writer.commit(&mixed_segment()).unwrap();
        let bytes = writer.storage.read_all().unwrap();

        assert_eq!(bytes.len(), complete.len() * 2);
        assert_eq!(&bytes[..complete.len()], complete);
        assert_eq!(recover(&bytes).unwrap().segments.len(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pty_stream_writer_rejects_committed_length_overflow_before_writing() {
        let mut writer = SegmentWriter {
            storage: FakeStorage::default(),
            committed_len: u64::MAX,
            poisoned: false,
        };
        assert_eq!(
            writer.commit(&mixed_segment()),
            Err(StreamError::CommittedLengthOverflow)
        );
        assert!(writer.storage.events.is_empty());
        assert!(writer.storage.bytes.is_empty());
    }
}
