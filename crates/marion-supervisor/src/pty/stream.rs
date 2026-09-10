//! Durable binary PTY stream primitives.

// This increment deliberately lands the format and durability machinery before its production
// callsite, so every item in this module is expected to remain dark until that integration lands.
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use marion_core::contract::AgentId;
use thiserror::Error;

const MAGIC: [u8; 8] = *b"MRNPTY01";
const VERSION: u16 = 1;
const HEADER_LEN: usize = MAGIC.len() + size_of::<u16>();
const FRAME_LEN_BYTES: usize = size_of::<u32>();
const CHECKSUM_BYTES: usize = 32;
const FRAME_FIXED_BYTES: usize = 1 + (3 * size_of::<u64>()) + size_of::<u32>();
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_RECOVERY_BYTES: usize = 16 * 1024 * 1024;
/// Leave room after display capture stops for ordered input evidence, geometry, and terminal End.
/// Exhausting this display budget is an honest truncation boundary, not a writer failure.
const SESSION_CONTROL_RESERVE_BYTES: usize = 1024 * 1024;
const SESSION_OUTPUT_LIMIT_BYTES: usize = MAX_RECOVERY_BYTES - SESSION_CONTROL_RESERVE_BYTES;
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
const DISPLAY_INCOMPLETE_KIND: u8 = 5;

const SESSION_MAGIC: [u8; 8] = *b"MRNPTS01";
const LEGACY_SESSION_VERSION: u16 = 2;
const SESSION_VERSION: u16 = 3;
const SESSION_ID_BYTES: usize = 16;
const AGENT_BINDING_BYTES: usize = 32;
/// Reserved zero bytes retain the fixed v2 header length without persisting a secret-verification
/// key. Readers accept v2's historical non-zero salt, but every v3 writer emits zeros.
const PRIVACY_RESERVED_BYTES: usize = 32;
const SESSION_HEADER_PREFIX_LEN: usize = SESSION_MAGIC.len()
    + size_of::<u16>()
    + (2 * size_of::<u16>())
    + SESSION_ID_BYTES
    + AGENT_BINDING_BYTES
    + PRIVACY_RESERVED_BYTES;
const SESSION_HEADER_LEN: usize = SESSION_HEADER_PREFIX_LEN + CHECKSUM_BYTES;
const AGENT_BINDING_DOMAIN: &[u8] = b"marion.pty.agent-binding.v1\0";
const LEGACY_TERMINAL_OUTCOME_BYTES: usize = 11;
const TERMINAL_OUTCOME_BYTES: usize = 12;
const EXIT_CODE_PRESENT: u8 = 1 << 0;
const SIGNAL_PRESENT: u8 = 1 << 1;
const TIMED_OUT: u8 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordFormat {
    LegacyV1,
    SessionV2,
    SessionV3,
}

impl RecordFormat {
    fn for_session_version(version: u16) -> Result<Self, StreamError> {
        match version {
            LEGACY_SESSION_VERSION => Ok(Self::SessionV2),
            SESSION_VERSION => Ok(Self::SessionV3),
            other => Err(StreamError::UnsupportedVersion(other)),
        }
    }
}

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
    #[error("PTY stream display output reached its bounded {max}-byte budget")]
    OutputBudgetExhausted { max: usize },
    #[error("PTY stream recovery exceeds maximum {max} segments")]
    TooManySegments { max: usize },
    #[error("PTY stream committed length overflow")]
    CommittedLengthOverflow,
    #[error("PTY stream writer is poisoned after a prior durability failure")]
    WriterPoisoned,
    #[error("PTY stream session id entropy failed: {0}")]
    Entropy(String),
    #[error("PTY stream session header checksum mismatch")]
    HeaderChecksumMismatch,
    #[error("PTY stream session is already ended")]
    SessionEnded,
    #[error("PTY stream path has no file name")]
    InvalidCastPath,
    #[error("PTY stream parent directory is not private and owned by the effective user")]
    UnsafeParent,
    #[error("PTY stream file is not a private regular file owned by the effective user")]
    UnsafeFile,
    #[error("PTY stream path changed while it was being opened")]
    PathChanged,
    #[error("PTY stream already has a live writer")]
    WriterLocked,
    #[error("PTY stream session header does not match the expected identity or geometry")]
    HeaderMismatch,
    #[error("PTY stream counter overflow")]
    CounterOverflow,
    #[error("PTY stream terminal outcome is internally inconsistent")]
    InvalidTerminalOutcome,
    #[error("PTY stream session format requires a typed terminal outcome")]
    MissingTerminalOutcome,
    #[error("legacy PTY stream segments cannot contain a typed session outcome")]
    TypedTerminalOutcomeInLegacySegment,
}

impl From<io::Error> for StreamError {
    fn from(error: io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<rustix::io::Errno> for StreamError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Storage(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReaderDisposition {
    CleanEof,
    ForcedStop,
    ReadError,
}

impl ReaderDisposition {
    fn encode(self) -> u8 {
        match self {
            Self::CleanEof => 1,
            Self::ForcedStop => 2,
            Self::ReadError => 3,
        }
    }

    fn decode(encoded: u8) -> Result<Self, StreamError> {
        match encoded {
            1 => Ok(Self::CleanEof),
            2 => Ok(Self::ForcedStop),
            3 => Ok(Self::ReadError),
            _ => Err(StreamError::InvalidTerminalOutcome),
        }
    }
}

/// Durable terminal evidence. The status representation is deliberately validated rather than a
/// permissive bag of options: a normal terminal observation has exactly one of an exit code or a
/// signal, while a timeout may legitimately have neither when the final status was unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalOutcome {
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) reader: ReaderDisposition,
    pub(crate) cast_complete: bool,
    pub(crate) stream_complete: bool,
}

impl TerminalOutcome {
    pub(crate) fn validate(self) -> Result<(), StreamError> {
        if self.exit_code.is_some() && self.signal.is_some() {
            return Err(StreamError::InvalidTerminalOutcome);
        }
        if !self.timed_out && self.exit_code.is_none() && self.signal.is_none() {
            return Err(StreamError::InvalidTerminalOutcome);
        }
        Ok(())
    }

    pub(crate) fn replay_eligible(self) -> bool {
        self.validate().is_ok()
            && self.reader == ReaderDisposition::CleanEof
            && self.cast_complete
            && self.stream_complete
    }

    fn encode(self) -> Result<[u8; TERMINAL_OUTCOME_BYTES], StreamError> {
        self.validate()?;
        let mut encoded = [0; TERMINAL_OUTCOME_BYTES];
        if let Some(exit_code) = self.exit_code {
            encoded[0] |= EXIT_CODE_PRESENT;
            encoded[1..5].copy_from_slice(&exit_code.to_le_bytes());
        }
        if let Some(signal) = self.signal {
            encoded[0] |= SIGNAL_PRESENT;
            encoded[5..9].copy_from_slice(&signal.to_le_bytes());
        }
        if self.timed_out {
            encoded[0] |= TIMED_OUT;
        }
        encoded[9] = self.reader.encode();
        encoded[10] = u8::from(self.cast_complete);
        encoded[11] = u8::from(self.stream_complete);
        Ok(encoded)
    }

    fn decode(encoded: &[u8], stream_complete: bool) -> Result<Self, StreamError> {
        if encoded.len() < LEGACY_TERMINAL_OUTCOME_BYTES
            || encoded.len() > TERMINAL_OUTCOME_BYTES
            || encoded[0] & !(EXIT_CODE_PRESENT | SIGNAL_PRESENT | TIMED_OUT) != 0
            || encoded[10] > 1
            || encoded.get(11).is_some_and(|complete| *complete > 1)
        {
            return Err(StreamError::InvalidTerminalOutcome);
        }
        let exit_code = if encoded[0] & EXIT_CODE_PRESENT != 0 {
            Some(i32::from_le_bytes(encoded[1..5].try_into().unwrap()))
        } else {
            if encoded[1..5] != [0; 4] {
                return Err(StreamError::InvalidTerminalOutcome);
            }
            None
        };
        let signal = if encoded[0] & SIGNAL_PRESENT != 0 {
            Some(i32::from_le_bytes(encoded[5..9].try_into().unwrap()))
        } else {
            if encoded[5..9] != [0; 4] {
                return Err(StreamError::InvalidTerminalOutcome);
            }
            None
        };
        let outcome = Self {
            exit_code,
            signal,
            timed_out: encoded[0] & TIMED_OUT != 0,
            reader: ReaderDisposition::decode(encoded[9])?,
            cast_complete: encoded[10] != 0,
            stream_complete: encoded
                .get(11)
                .map_or(stream_complete, |complete| *complete != 0),
        };
        outcome.validate()?;
        Ok(outcome)
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
    /// A v3 control record proving display evidence is incomplete, either because output capture
    /// reached its budget or recovery discarded a torn record whose contents are unknowable.
    DisplayIncomplete,
    /// The empty v1 segment terminator. Versioned session format rejects this representation.
    LegacyEnd,
    /// The session terminator, including the lifecycle evidence needed to judge replayability.
    End(TerminalOutcome),
}

impl Record {
    fn encoded_len(&self) -> Result<usize, StreamError> {
        self.encoded_len_for(RecordFormat::LegacyV1)
    }

    fn encoded_len_for(&self, format: RecordFormat) -> Result<usize, StreamError> {
        let payload_len = match &self.kind {
            RecordKind::Output(bytes) => bytes.len(),
            RecordKind::Resize { .. } => 2 * size_of::<u16>(),
            RecordKind::InputEvidence { input_seq, .. } => {
                if *input_seq != self.input_seq {
                    return Err(StreamError::InputSequenceMismatch);
                }
                match format {
                    RecordFormat::SessionV2 => size_of::<u32>() + CHECKSUM_BYTES,
                    RecordFormat::LegacyV1 | RecordFormat::SessionV3 => size_of::<u32>(),
                }
            }
            RecordKind::DisplayIncomplete if format == RecordFormat::SessionV3 => 0,
            RecordKind::DisplayIncomplete => return Err(StreamError::InvalidRecordPayload),
            RecordKind::LegacyEnd if format == RecordFormat::LegacyV1 => 0,
            RecordKind::LegacyEnd => return Err(StreamError::MissingTerminalOutcome),
            RecordKind::End(outcome) => {
                if format == RecordFormat::LegacyV1 {
                    return Err(StreamError::TypedTerminalOutcomeInLegacySegment);
                }
                outcome.validate()?;
                match format {
                    RecordFormat::SessionV2 => LEGACY_TERMINAL_OUTCOME_BYTES,
                    RecordFormat::SessionV3 => TERMINAL_OUTCOME_BYTES,
                    RecordFormat::LegacyV1 => unreachable!(),
                }
            }
        };
        encoded_record_len(payload_len)
    }

    /// `u32 frame_len` (little-endian), then record bytes, followed by a BLAKE3 checksum of those
    /// record bytes. The record itself uses only explicit little-endian integer encodings.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, StreamError> {
        self.encode_for(RecordFormat::LegacyV1)
    }

    fn encode_for(&self, format: RecordFormat) -> Result<Vec<u8>, StreamError> {
        let encoded_len = self.encoded_len_for(format)?;
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
                let mut payload = Vec::with_capacity(match format {
                    RecordFormat::SessionV2 => size_of::<u32>() + CHECKSUM_BYTES,
                    RecordFormat::LegacyV1 | RecordFormat::SessionV3 => size_of::<u32>(),
                });
                payload.extend_from_slice(&byte_len.to_le_bytes());
                if format == RecordFormat::SessionV2 {
                    // v2's historical digest is retired. This path exists only to keep the frozen
                    // representation explicit; current writers always use v3.
                    payload.extend_from_slice(&[0; CHECKSUM_BYTES]);
                }
                (INPUT_EVIDENCE_KIND, payload)
            }
            RecordKind::DisplayIncomplete => (DISPLAY_INCOMPLETE_KIND, Vec::new()),
            RecordKind::LegacyEnd => (END_KIND, Vec::new()),
            RecordKind::End(outcome) => {
                let encoded = outcome.encode()?;
                let len = match format {
                    RecordFormat::SessionV2 => LEGACY_TERMINAL_OUTCOME_BYTES,
                    RecordFormat::SessionV3 => TERMINAL_OUTCOME_BYTES,
                    RecordFormat::LegacyV1 => unreachable!(),
                };
                (END_KIND, encoded[..len].to_vec())
            }
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

        let mut encoded = Vec::with_capacity(encoded_len);
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
        Self::decode_for(encoded, RecordFormat::LegacyV1)
    }

    fn decode_for(encoded: &[u8], format: RecordFormat) -> Result<Self, StreamError> {
        let record = checked_record_bytes(encoded)?;
        let kind = record[0];
        let record_seq = u64::from_le_bytes(record[1..9].try_into().unwrap());
        let display_seq = u64::from_le_bytes(record[9..17].try_into().unwrap());
        let input_seq = u64::from_le_bytes(record[17..25].try_into().unwrap());
        let payload_len = u32::from_le_bytes(record[25..29].try_into().unwrap()) as usize;
        if payload_len != record.len() - FRAME_FIXED_BYTES {
            return Err(StreamError::InvalidRecordPayload);
        }
        let payload = &record[FRAME_FIXED_BYTES..];
        Ok(Self {
            record_seq,
            display_seq,
            input_seq,
            kind: decode_record_kind(kind, payload, input_seq, format)?,
        })
    }
}

/// Validate a frame's length prefix, bounds, and checksum; return the record bytes it protects.
fn checked_record_bytes(encoded: &[u8]) -> Result<&[u8], StreamError> {
    if encoded.len() < FRAME_LEN_BYTES + CHECKSUM_BYTES {
        return Err(StreamError::TruncatedFrame);
    }
    let record_len = u32::from_le_bytes(encoded[..FRAME_LEN_BYTES].try_into().unwrap()) as usize;
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
    Ok(record)
}

/// Interpret a checked record's kind byte and payload under `format`.
fn decode_record_kind(
    kind: u8,
    payload: &[u8],
    input_seq: u64,
    format: RecordFormat,
) -> Result<RecordKind, StreamError> {
    match kind {
        OUTPUT_KIND => Ok(RecordKind::Output(payload.to_vec())),
        RESIZE_KIND => decode_resize_payload(payload),
        INPUT_EVIDENCE_KIND => decode_input_evidence_payload(payload, input_seq, format),
        DISPLAY_INCOMPLETE_KIND => decode_display_incomplete_payload(payload, format),
        END_KIND => decode_end_payload(payload, format),
        other => Err(StreamError::UnknownRecordKind(other)),
    }
}

fn decode_resize_payload(payload: &[u8]) -> Result<RecordKind, StreamError> {
    if payload.len() != 4 {
        return Err(StreamError::InvalidRecordPayload);
    }
    Ok(RecordKind::Resize {
        rows: u16::from_le_bytes(payload[..2].try_into().unwrap()),
        cols: u16::from_le_bytes(payload[2..].try_into().unwrap()),
    })
}

/// The on-disk width of an input-evidence payload under `format`.
fn input_evidence_payload_len(format: RecordFormat) -> usize {
    match format {
        RecordFormat::SessionV2 => size_of::<u32>() + CHECKSUM_BYTES,
        RecordFormat::LegacyV1 | RecordFormat::SessionV3 => size_of::<u32>(),
    }
}

fn decode_input_evidence_payload(
    payload: &[u8],
    input_seq: u64,
    format: RecordFormat,
) -> Result<RecordKind, StreamError> {
    if payload.len() != input_evidence_payload_len(format) {
        return Err(StreamError::InvalidRecordPayload);
    }
    // v2 appended a keyed content fingerprint after this length. Its key was stored in the same
    // header, so exposing it preserved offline guess verification. Recover its honest
    // ordering/length while deliberately discarding the unsafe legacy digest.
    Ok(RecordKind::InputEvidence {
        input_seq,
        byte_len: u32::from_le_bytes(payload[..4].try_into().unwrap()),
    })
}

fn decode_display_incomplete_payload(
    payload: &[u8],
    format: RecordFormat,
) -> Result<RecordKind, StreamError> {
    if format == RecordFormat::SessionV3 && payload.is_empty() {
        return Ok(RecordKind::DisplayIncomplete);
    }
    Err(StreamError::InvalidRecordPayload)
}

/// The on-disk width of a typed terminal outcome under a versioned session `format`.
fn typed_terminal_outcome_len(format: RecordFormat) -> usize {
    match format {
        RecordFormat::SessionV2 => LEGACY_TERMINAL_OUTCOME_BYTES,
        RecordFormat::SessionV3 => TERMINAL_OUTCOME_BYTES,
        RecordFormat::LegacyV1 => unreachable!(),
    }
}

fn decode_end_payload(payload: &[u8], format: RecordFormat) -> Result<RecordKind, StreamError> {
    if format == RecordFormat::LegacyV1 {
        return if payload.is_empty() {
            Ok(RecordKind::LegacyEnd)
        } else {
            Err(StreamError::InvalidRecordPayload)
        };
    }
    if payload.is_empty() {
        return Err(StreamError::MissingTerminalOutcome);
    }
    if payload.len() != typed_terminal_outcome_len(format) {
        return Err(StreamError::InvalidRecordPayload);
    }
    Ok(RecordKind::End(TerminalOutcome::decode(
        payload,
        format == RecordFormat::SessionV2,
    )?))
}

fn encoded_record_len(payload_len: usize) -> Result<usize, StreamError> {
    let record_len =
        FRAME_FIXED_BYTES
            .checked_add(payload_len)
            .ok_or(StreamError::RecordTooLarge {
                actual: usize::MAX,
                max: MAX_RECORD_BYTES,
            })?;
    if record_len > MAX_RECORD_BYTES {
        return Err(StreamError::RecordTooLarge {
            actual: record_len,
            max: MAX_RECORD_BYTES,
        });
    }
    FRAME_LEN_BYTES
        .checked_add(record_len)
        .and_then(|len| len.checked_add(CHECKSUM_BYTES))
        .ok_or(StreamError::CommittedLengthOverflow)
}

/// An in-memory, fully committed PTY recording segment. A trailer makes a segment self-validating
/// before a future writer exposes it on disk: it repeats the format identity and binds every byte
/// before it, including the header and each per-frame checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) records: Vec<Record>,
}

impl Segment {
    fn encoded_len(&self) -> Result<usize, StreamError> {
        validate_records(&self.records)?;
        self.records
            .iter()
            .try_fold(HEADER_LEN + TRAILER_LEN, |len, record| {
                len.checked_add(record.encoded_len()?)
                    .ok_or(StreamError::CommittedLengthOverflow)
            })
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, StreamError> {
        let counters = validate_records(&self.records)?;
        let mut encoded = Vec::with_capacity(self.encoded_len()?);
        encoded.extend_from_slice(&Header { version: VERSION }.encode());
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
        let encoded_len = segment.encoded_len()?;
        let byte_len =
            u64::try_from(encoded_len).map_err(|_| StreamError::CommittedLengthOverflow)?;
        let next_committed_len = self
            .committed_len
            .checked_add(byte_len)
            .ok_or(StreamError::CommittedLengthOverflow)?;
        if next_committed_len > MAX_RECOVERY_BYTES as u64 {
            return Err(StreamError::RecoveryTooLarge {
                max: MAX_RECOVERY_BYTES,
            });
        }
        let bytes = segment.encode()?;
        debug_assert_eq!(bytes.len(), encoded_len);
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
        if self.storage.len()? > MAX_RECOVERY_BYTES as u64 {
            return Err(StreamError::RecoveryTooLarge {
                max: MAX_RECOVERY_BYTES,
            });
        }
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
        let is_end = matches!(&record.kind, RecordKind::LegacyEnd);
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
        if matches!(&record.kind, RecordKind::End(_)) {
            return Err(StreamError::TypedTerminalOutcomeInLegacySegment);
        }
        ended = matches!(&record.kind, RecordKind::LegacyEnd);
    }
    if !ended {
        return Err(StreamError::MissingEnd);
    }
    Ok(counters)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionHeader {
    pub(crate) version: u16,
    pub(crate) initial_cols: u16,
    pub(crate) initial_rows: u16,
    pub(crate) session_id: [u8; SESSION_ID_BYTES],
    pub(crate) agent_binding: [u8; AGENT_BINDING_BYTES],
    privacy_reserved: [u8; PRIVACY_RESERVED_BYTES],
}

impl SessionHeader {
    fn new(
        agent_id: &AgentId,
        initial_size: super::WinSize,
        session_id: [u8; SESSION_ID_BYTES],
    ) -> Self {
        let mut binding = blake3::Hasher::new();
        binding.update(AGENT_BINDING_DOMAIN);
        binding.update(&(agent_id.0.len() as u64).to_le_bytes());
        binding.update(agent_id.0.as_bytes());
        Self {
            version: SESSION_VERSION,
            initial_cols: initial_size.cols,
            initial_rows: initial_size.rows,
            session_id,
            agent_binding: *binding.finalize().as_bytes(),
            privacy_reserved: [0; PRIVACY_RESERVED_BYTES],
        }
    }

    fn encode(&self) -> [u8; SESSION_HEADER_LEN] {
        let mut encoded = [0; SESSION_HEADER_LEN];
        let mut offset = 0;
        encoded[offset..offset + SESSION_MAGIC.len()].copy_from_slice(&SESSION_MAGIC);
        offset += SESSION_MAGIC.len();
        encoded[offset..offset + size_of::<u16>()].copy_from_slice(&self.version.to_le_bytes());
        offset += size_of::<u16>();
        encoded[offset..offset + size_of::<u16>()]
            .copy_from_slice(&self.initial_cols.to_le_bytes());
        offset += size_of::<u16>();
        encoded[offset..offset + size_of::<u16>()]
            .copy_from_slice(&self.initial_rows.to_le_bytes());
        offset += size_of::<u16>();
        encoded[offset..offset + SESSION_ID_BYTES].copy_from_slice(&self.session_id);
        offset += SESSION_ID_BYTES;
        encoded[offset..offset + AGENT_BINDING_BYTES].copy_from_slice(&self.agent_binding);
        offset += AGENT_BINDING_BYTES;
        encoded[offset..offset + PRIVACY_RESERVED_BYTES].copy_from_slice(&self.privacy_reserved);
        let checksum = blake3::hash(&encoded[..SESSION_HEADER_PREFIX_LEN]);
        encoded[SESSION_HEADER_PREFIX_LEN..].copy_from_slice(checksum.as_bytes());
        encoded
    }

    fn decode(encoded: &[u8]) -> Result<Self, StreamError> {
        if encoded.len() != SESSION_HEADER_LEN {
            return Err(StreamError::InvalidHeaderLength {
                actual: encoded.len(),
                expected: SESSION_HEADER_LEN,
            });
        }
        if encoded[..SESSION_MAGIC.len()] != SESSION_MAGIC {
            return Err(StreamError::BadMagic);
        }
        if blake3::hash(&encoded[..SESSION_HEADER_PREFIX_LEN]).as_bytes()
            != &encoded[SESSION_HEADER_PREFIX_LEN..]
        {
            return Err(StreamError::HeaderChecksumMismatch);
        }
        let mut offset = SESSION_MAGIC.len();
        let version = u16::from_le_bytes(encoded[offset..offset + 2].try_into().unwrap());
        offset += 2;
        if !matches!(version, LEGACY_SESSION_VERSION | SESSION_VERSION) {
            return Err(StreamError::UnsupportedVersion(version));
        }
        let initial_cols = u16::from_le_bytes(encoded[offset..offset + 2].try_into().unwrap());
        offset += 2;
        let initial_rows = u16::from_le_bytes(encoded[offset..offset + 2].try_into().unwrap());
        offset += 2;
        let session_id = encoded[offset..offset + SESSION_ID_BYTES]
            .try_into()
            .unwrap();
        offset += SESSION_ID_BYTES;
        let agent_binding = encoded[offset..offset + AGENT_BINDING_BYTES]
            .try_into()
            .unwrap();
        offset += AGENT_BINDING_BYTES;
        let encoded_reserved: [u8; PRIVACY_RESERVED_BYTES] = encoded
            [offset..offset + PRIVACY_RESERVED_BYTES]
            .try_into()
            .unwrap();
        if version == SESSION_VERSION && encoded_reserved != [0; PRIVACY_RESERVED_BYTES] {
            return Err(StreamError::HeaderMismatch);
        }
        Ok(Self {
            version,
            initial_cols,
            initial_rows,
            session_id,
            agent_binding,
            // Discard v2's historical content-verification key at the decoding boundary.
            privacy_reserved: [0; PRIVACY_RESERVED_BYTES],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRecovery {
    pub(crate) header: SessionHeader,
    pub(crate) records: Vec<Record>,
    pub(crate) truncate_to: Option<usize>,
}

pub(crate) fn stream_path_for_cast(cast_path: &Path) -> Result<PathBuf, StreamError> {
    if cast_path.file_name().is_none() {
        return Err(StreamError::InvalidCastPath);
    }
    Ok(cast_path.with_extension("stream"))
}

struct PinnedStreamPath {
    parent: OwnedFd,
    name: OsString,
}

impl PinnedStreamPath {
    fn open(cast_path: &Path) -> Result<Self, StreamError> {
        let path = stream_path_for_cast(cast_path)?;
        let parent_path = path.parent().ok_or(StreamError::InvalidCastPath)?;
        let name = path
            .file_name()
            .ok_or(StreamError::InvalidCastPath)?
            .to_owned();
        let parent = rustix::fs::open(
            parent_path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let stat = rustix::fs::fstat(&parent)?;
        if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
            return Err(StreamError::UnsafeParent);
        }
        Ok(Self { parent, name })
    }

    fn open_new(&self) -> Result<File, StreamError> {
        let fd = rustix::fs::openat(
            &self.parent,
            &self.name,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        rustix::fs::fchmod(&fd, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR)?;
        let file = File::from(fd);
        validate_stream_file(&file)?;
        lock_writer(&file)?;
        Ok(file)
    }

    fn open_existing(&self) -> Result<File, StreamError> {
        let fd = rustix::fs::openat(
            &self.parent,
            &self.name,
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let file = File::from(fd);
        validate_stream_file(&file)?;
        lock_writer(&file)?;
        self.ensure_current(&file)?;
        Ok(file)
    }

    fn ensure_current(&self, file: &File) -> Result<(), StreamError> {
        let opened = rustix::fs::fstat(file)?;
        let current = rustix::fs::statat(
            &self.parent,
            &self.name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )?;
        if opened.st_dev != current.st_dev || opened.st_ino != current.st_ino {
            return Err(StreamError::PathChanged);
        }
        Ok(())
    }

    fn remove_if_current(&self, file: &File) {
        if self.ensure_current(file).is_ok() {
            let _ = rustix::fs::unlinkat(&self.parent, &self.name, rustix::fs::AtFlags::empty());
            let _ = rustix::fs::fsync(&self.parent);
        }
    }
}

fn validate_stream_file(file: &File) -> Result<(), StreamError> {
    let stat = rustix::fs::fstat(file)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
    {
        return Err(StreamError::UnsafeFile);
    }
    Ok(())
}

fn lock_writer(file: &File) -> Result<(), StreamError> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        if error == rustix::io::Errno::AGAIN {
            StreamError::WriterLocked
        } else {
            StreamError::Storage(error.to_string())
        }
    })
}

pub(crate) fn recover_session_bytes(encoded: &[u8]) -> Result<SessionRecovery, StreamError> {
    if encoded.len() < SESSION_HEADER_LEN {
        return Err(StreamError::InvalidHeaderLength {
            actual: encoded.len(),
            expected: SESSION_HEADER_LEN,
        });
    }
    if encoded.len() > MAX_RECOVERY_BYTES {
        return Err(StreamError::RecoveryTooLarge {
            max: MAX_RECOVERY_BYTES,
        });
    }
    let header = SessionHeader::decode(&encoded[..SESSION_HEADER_LEN])?;
    let format = RecordFormat::for_session_version(header.version)?;
    let mut records = Vec::new();
    let mut offset = SESSION_HEADER_LEN;
    let mut counters = TrailerCounters {
        record_seq: 0,
        display_seq: 0,
        input_seq: 0,
    };
    let mut ended = false;
    let mut display_incomplete = false;
    while offset < encoded.len() {
        if ended {
            return Err(StreamError::RecordAfterEnd);
        }
        if encoded.len() - offset < FRAME_LEN_BYTES {
            return Ok(SessionRecovery {
                header,
                records,
                truncate_to: Some(offset),
            });
        }
        let record_len = u32::from_le_bytes(
            encoded[offset..offset + FRAME_LEN_BYTES]
                .try_into()
                .unwrap(),
        ) as usize;
        if !(FRAME_FIXED_BYTES..=MAX_RECORD_BYTES).contains(&record_len) {
            return Err(StreamError::RecordTooLarge {
                actual: record_len,
                max: MAX_RECORD_BYTES,
            });
        }
        let frame_end = offset
            .checked_add(FRAME_LEN_BYTES + record_len + CHECKSUM_BYTES)
            .ok_or(StreamError::CommittedLengthOverflow)?;
        if frame_end > encoded.len() {
            return Ok(SessionRecovery {
                header,
                records,
                truncate_to: Some(offset),
            });
        }
        let record = Record::decode_for(&encoded[offset..frame_end], format)?;
        validate_next_record(&record, &mut counters, &mut display_incomplete)?;
        ended = matches!(record.kind, RecordKind::End(_));
        records.push(record);
        offset = frame_end;
    }
    Ok(SessionRecovery {
        header,
        records,
        truncate_to: None,
    })
}

fn validate_next_record(
    record: &Record,
    counters: &mut TrailerCounters,
    display_incomplete: &mut bool,
) -> Result<(), StreamError> {
    let expected_record = counters
        .record_seq
        .checked_add(1)
        .ok_or(StreamError::CounterOverflow)?;
    if record.record_seq != expected_record {
        return Err(StreamError::RecordSequenceGap {
            expected: expected_record,
            actual: record.record_seq,
        });
    }
    let advances_display = matches!(
        &record.kind,
        RecordKind::Output(_) | RecordKind::Resize { .. }
    );
    let expected_display = counters
        .display_seq
        .checked_add(u64::from(advances_display))
        .ok_or(StreamError::CounterOverflow)?;
    if record.display_seq != expected_display {
        return Err(StreamError::DisplaySequenceGap {
            expected: expected_display,
            actual: record.display_seq,
        });
    }
    let advances_input = matches!(&record.kind, RecordKind::InputEvidence { .. });
    let expected_input = counters
        .input_seq
        .checked_add(u64::from(advances_input))
        .ok_or(StreamError::CounterOverflow)?;
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
    if *display_incomplete && matches!(&record.kind, RecordKind::Output(_)) {
        return Err(StreamError::InvalidRecordPayload);
    }
    if matches!(&record.kind, RecordKind::DisplayIncomplete) {
        if *display_incomplete {
            return Err(StreamError::InvalidRecordPayload);
        }
        *display_incomplete = true;
    }
    if let RecordKind::End(outcome) = &record.kind
        && *display_incomplete
        && outcome.stream_complete
    {
        return Err(StreamError::InvalidTerminalOutcome);
    }
    counters.record_seq = record.record_seq;
    counters.display_seq = record.display_seq;
    counters.input_seq = record.input_seq;
    Ok(())
}

pub(crate) struct SessionWriter {
    file: File,
    header: SessionHeader,
    counters: TrailerCounters,
    committed_len: u64,
    output_limit: u64,
    display_incomplete: bool,
    ended: bool,
    poisoned: bool,
}

impl SessionWriter {
    pub(crate) fn create(
        cast_path: &Path,
        agent_id: &AgentId,
        initial_size: super::WinSize,
    ) -> Result<Self, StreamError> {
        let mut session_id = [0; SESSION_ID_BYTES];
        getrandom::fill(&mut session_id)
            .map_err(|error| StreamError::Entropy(error.to_string()))?;
        Self::create_prepared(
            cast_path,
            SessionHeader::new(agent_id, initial_size, session_id),
            Write::write_all,
        )
    }

    fn create_with_session(
        cast_path: &Path,
        agent_id: &AgentId,
        initial_size: super::WinSize,
        session_id: [u8; SESSION_ID_BYTES],
    ) -> Result<Self, StreamError> {
        let header = SessionHeader::new(agent_id, initial_size, session_id);
        Self::create_prepared(cast_path, header, |file, encoded| {
            Write::write_all(file, encoded)
        })
    }

    fn create_prepared(
        cast_path: &Path,
        header: SessionHeader,
        write_header: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
    ) -> Result<Self, StreamError> {
        let path = PinnedStreamPath::open(cast_path)?;
        let mut file = path.open_new()?;
        let initialized = (|| -> Result<(), StreamError> {
            write_header(&mut file, &header.encode())?;
            file.sync_all()?;
            path.ensure_current(&file)?;
            rustix::fs::fsync(&path.parent)?;
            Ok(())
        })();
        if let Err(error) = initialized {
            path.remove_if_current(&file);
            return Err(error);
        }
        Ok(Self {
            file,
            header,
            counters: TrailerCounters {
                record_seq: 0,
                display_seq: 0,
                input_seq: 0,
            },
            committed_len: SESSION_HEADER_LEN as u64,
            output_limit: SESSION_OUTPUT_LIMIT_BYTES as u64,
            display_incomplete: false,
            ended: false,
            poisoned: false,
        })
    }

    pub(crate) fn reopen(
        cast_path: &Path,
        agent_id: &AgentId,
        initial_size: super::WinSize,
        session_id: [u8; SESSION_ID_BYTES],
    ) -> Result<Self, StreamError> {
        Self::reopen_prepared(
            cast_path,
            agent_id,
            initial_size,
            session_id,
            |file, offset, marker| {
                file.seek(SeekFrom::Start(offset))?;
                Write::write_all(file, marker)?;
                file.sync_data()
            },
        )
    }

    fn reopen_prepared(
        cast_path: &Path,
        agent_id: &AgentId,
        initial_size: super::WinSize,
        session_id: [u8; SESSION_ID_BYTES],
        persist_repair: impl FnOnce(&mut File, u64, &[u8]) -> io::Result<()>,
    ) -> Result<Self, StreamError> {
        let path = PinnedStreamPath::open(cast_path)?;
        let mut file = path.open_existing()?;
        let file_len = file.metadata()?.len();
        if file_len > MAX_RECOVERY_BYTES as u64 {
            return Err(StreamError::RecoveryTooLarge {
                max: MAX_RECOVERY_BYTES,
            });
        }
        let mut encoded = Vec::new();
        (&mut file)
            .take(MAX_RECOVERY_BYTES as u64 + 1)
            .read_to_end(&mut encoded)?;
        if encoded.len() > MAX_RECOVERY_BYTES {
            return Err(StreamError::RecoveryTooLarge {
                max: MAX_RECOVERY_BYTES,
            });
        }
        let recovery = recover_session_bytes(&encoded)?;
        let expected = SessionHeader::new(agent_id, initial_size, session_id);
        if recovery.header.version != SESSION_VERSION
            || recovery.header.initial_cols != expected.initial_cols
            || recovery.header.initial_rows != expected.initial_rows
            || recovery.header.session_id != expected.session_id
            || recovery.header.agent_binding != expected.agent_binding
        {
            return Err(StreamError::HeaderMismatch);
        }
        path.ensure_current(&file)?;
        let committed_len = recovery.truncate_to.unwrap_or(encoded.len());
        let recovered_torn_tail = recovery.truncate_to.is_some();
        if recovered_torn_tail {
            file.set_len(committed_len as u64)?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::Start(committed_len as u64))?;
        path.ensure_current(&file)?;
        let counters = recovery
            .records
            .last()
            .map(|record| TrailerCounters {
                record_seq: record.record_seq,
                display_seq: record.display_seq,
                input_seq: record.input_seq,
            })
            .unwrap_or(TrailerCounters {
                record_seq: 0,
                display_seq: 0,
                input_seq: 0,
            });
        let ended = recovery
            .records
            .last()
            .is_some_and(|record| matches!(record.kind, RecordKind::End(_)));
        let display_incomplete = recovery
            .records
            .iter()
            .any(|record| matches!(record.kind, RecordKind::DisplayIncomplete));
        let mut writer = Self {
            file,
            header: recovery.header,
            counters,
            committed_len: committed_len as u64,
            output_limit: SESSION_OUTPUT_LIMIT_BYTES as u64,
            display_incomplete,
            ended,
            poisoned: false,
        };
        if !writer.ended && !writer.display_incomplete {
            // An unended file is crash recovery, even when it ended exactly on a frame boundary:
            // terminal bytes may have been lost before they reached a durable Output record. Mark
            // it incomplete before admitting new appends. If marker persistence fails, the next
            // reopen sees the same unended prefix (or a partial marker), repairs it, and retries.
            let marker = writer.next_record(RecordKind::DisplayIncomplete)?;
            let encoded_marker = marker.encode_for(RecordFormat::SessionV3)?;
            if let Err(error) =
                persist_repair(&mut writer.file, writer.committed_len, &encoded_marker)
            {
                return Err(error.into());
            }
            let repaired_len = writer
                .committed_len
                .checked_add(encoded_marker.len() as u64)
                .ok_or(StreamError::CommittedLengthOverflow)?;
            writer.file.set_len(repaired_len)?;
            writer.file.sync_data()?;
            writer.counters = TrailerCounters {
                record_seq: marker.record_seq,
                display_seq: marker.display_seq,
                input_seq: marker.input_seq,
            };
            writer.committed_len = repaired_len;
            writer.display_incomplete = true;
        }
        writer.file.seek(SeekFrom::Start(writer.committed_len))?;
        Ok(writer)
    }

    pub(crate) fn append_output(&mut self, bytes: &[u8]) -> Result<(), StreamError> {
        encoded_record_len(bytes.len())?;
        if self.display_incomplete {
            return Err(StreamError::OutputBudgetExhausted {
                max: usize::try_from(self.output_limit).unwrap_or(usize::MAX),
            });
        }
        let kind = RecordKind::Output(bytes.to_vec());
        let encoded_len = self.next_encoded_len(&kind)?;
        if self
            .committed_len
            .checked_add(encoded_len)
            .ok_or(StreamError::CommittedLengthOverflow)?
            > self.output_limit
        {
            self.append(RecordKind::DisplayIncomplete)?;
            return Err(StreamError::OutputBudgetExhausted {
                max: usize::try_from(self.output_limit).unwrap_or(usize::MAX),
            });
        }
        self.append_display(kind)
    }

    pub(crate) fn append_resize(&mut self, size: super::WinSize) -> Result<(), StreamError> {
        self.append_display(RecordKind::Resize {
            rows: size.rows,
            cols: size.cols,
        })
    }

    pub(crate) fn append_input_evidence(&mut self, byte_len: usize) -> Result<(), StreamError> {
        let byte_len = u32::try_from(byte_len).map_err(|_| StreamError::RecordTooLarge {
            actual: byte_len,
            max: u32::MAX as usize,
        })?;
        let input_seq = self
            .counters
            .input_seq
            .checked_add(1)
            .ok_or(StreamError::CounterOverflow)?;
        self.append(RecordKind::InputEvidence {
            input_seq,
            byte_len,
        })
    }

    pub(crate) fn seal(mut self, mut outcome: TerminalOutcome) -> Result<(), StreamError> {
        outcome.stream_complete &= !self.display_incomplete;
        outcome.validate()?;
        self.append(RecordKind::End(outcome))
    }

    fn append_display(&mut self, kind: RecordKind) -> Result<(), StreamError> {
        self.append(kind)
    }

    fn append(&mut self, kind: RecordKind) -> Result<(), StreamError> {
        if self.poisoned {
            return Err(StreamError::WriterPoisoned);
        }
        if self.ended {
            return Err(StreamError::SessionEnded);
        }
        let record = self.next_record(kind)?;
        let record_seq = record.record_seq;
        let display_seq = record.display_seq;
        let input_seq = record.input_seq;
        let encoded_len = record.encoded_len_for(RecordFormat::SessionV3)?;
        let encoded_len_u64 =
            u64::try_from(encoded_len).map_err(|_| StreamError::CommittedLengthOverflow)?;
        let next_committed_len = self
            .committed_len
            .checked_add(encoded_len_u64)
            .ok_or(StreamError::CommittedLengthOverflow)?;
        if next_committed_len > MAX_RECOVERY_BYTES as u64 {
            return Err(StreamError::RecoveryTooLarge {
                max: MAX_RECOVERY_BYTES,
            });
        }
        let encoded = record.encode_for(RecordFormat::SessionV3)?;
        debug_assert_eq!(encoded.len(), encoded_len);
        if let Err(error) =
            Write::write_all(&mut self.file, &encoded).and_then(|()| self.file.sync_data())
        {
            self.poisoned = true;
            return Err(error.into());
        }
        self.counters = TrailerCounters {
            record_seq,
            display_seq,
            input_seq,
        };
        self.committed_len = next_committed_len;
        self.display_incomplete |= matches!(record.kind, RecordKind::DisplayIncomplete);
        self.ended = matches!(record.kind, RecordKind::End(_));
        Ok(())
    }

    fn next_record(&self, kind: RecordKind) -> Result<Record, StreamError> {
        Ok(Record {
            record_seq: self
                .counters
                .record_seq
                .checked_add(1)
                .ok_or(StreamError::CounterOverflow)?,
            display_seq: self
                .counters
                .display_seq
                .checked_add(u64::from(matches!(
                    &kind,
                    RecordKind::Output(_) | RecordKind::Resize { .. }
                )))
                .ok_or(StreamError::CounterOverflow)?,
            input_seq: self
                .counters
                .input_seq
                .checked_add(u64::from(matches!(&kind, RecordKind::InputEvidence { .. })))
                .ok_or(StreamError::CounterOverflow)?,
            kind,
        })
    }

    fn next_encoded_len(&self, kind: &RecordKind) -> Result<u64, StreamError> {
        let record = self.next_record(kind.clone())?;
        u64::try_from(record.encoded_len_for(RecordFormat::SessionV3)?)
            .map_err(|_| StreamError::CommittedLengthOverflow)
    }

    #[cfg(test)]
    pub(super) fn limit_output_for_test(&mut self, limit: usize) {
        self.output_limit = limit as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

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

    /// Freeze record bytes independently of the production encoder so version-shape tests cannot
    /// pass merely because a writer and reader make the same mistake.
    fn frozen_record(
        record_seq: u64,
        display_seq: u64,
        input_seq: u64,
        kind: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let record_len = FRAME_FIXED_BYTES + payload.len();
        let mut encoded = Vec::with_capacity(FRAME_LEN_BYTES + record_len + CHECKSUM_BYTES);
        encoded.extend_from_slice(&(record_len as u32).to_le_bytes());
        encoded.push(kind);
        encoded.extend_from_slice(&record_seq.to_le_bytes());
        encoded.extend_from_slice(&display_seq.to_le_bytes());
        encoded.extend_from_slice(&input_seq.to_le_bytes());
        encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        encoded.extend_from_slice(payload);
        let checksum = blake3::hash(&encoded[FRAME_LEN_BYTES..]);
        encoded.extend_from_slice(checksum.as_bytes());
        encoded
    }

    fn frozen_session_header(
        version: u16,
        privacy_reserved: [u8; PRIVACY_RESERVED_BYTES],
    ) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(SESSION_HEADER_LEN);
        encoded.extend_from_slice(b"MRNPTS01");
        encoded.extend_from_slice(&version.to_le_bytes());
        encoded.extend_from_slice(&80_u16.to_le_bytes());
        encoded.extend_from_slice(&24_u16.to_le_bytes());
        encoded.extend_from_slice(&[0x91; SESSION_ID_BYTES]);
        encoded.extend_from_slice(&[0xbc; AGENT_BINDING_BYTES]);
        encoded.extend_from_slice(&privacy_reserved);
        assert_eq!(encoded.len(), SESSION_HEADER_PREFIX_LEN);
        let checksum = blake3::hash(&encoded);
        encoded.extend_from_slice(checksum.as_bytes());
        encoded
    }

    fn frozen_clean_end_payload(stream_complete: Option<bool>) -> Vec<u8> {
        let mut payload = vec![0; LEGACY_TERMINAL_OUTCOME_BYTES];
        payload[0] = EXIT_CODE_PRESENT;
        payload[9] = ReaderDisposition::CleanEof.encode();
        payload[10] = 1;
        if let Some(stream_complete) = stream_complete {
            payload.push(u8::from(stream_complete));
        }
        payload
    }

    #[test]
    fn frozen_v2_session_recovers_retired_input_evidence_and_short_end() {
        let mut encoded =
            frozen_session_header(LEGACY_SESSION_VERSION, [0xa5; PRIVACY_RESERVED_BYTES]);
        let mut input_payload = 7_u32.to_le_bytes().to_vec();
        input_payload.extend_from_slice(&[0x5a; CHECKSUM_BYTES]);
        encoded.extend_from_slice(&frozen_record(1, 0, 1, INPUT_EVIDENCE_KIND, &input_payload));
        encoded.extend_from_slice(&frozen_record(
            2,
            0,
            1,
            END_KIND,
            &frozen_clean_end_payload(None),
        ));

        let recovery = recover_session_bytes(&encoded).unwrap();
        assert_eq!(recovery.header.version, LEGACY_SESSION_VERSION);
        assert_eq!(
            recovery.records,
            vec![
                Record {
                    record_seq: 1,
                    display_seq: 0,
                    input_seq: 1,
                    kind: RecordKind::InputEvidence {
                        input_seq: 1,
                        byte_len: 7,
                    },
                },
                Record {
                    record_seq: 2,
                    display_seq: 0,
                    input_seq: 1,
                    kind: RecordKind::End(TerminalOutcome {
                        exit_code: Some(0),
                        signal: None,
                        timed_out: false,
                        reader: ReaderDisposition::CleanEof,
                        cast_complete: true,
                        stream_complete: true,
                    }),
                },
            ]
        );
    }

    #[test]
    fn v2_session_rejects_v3_record_shapes() {
        let mut short_input =
            frozen_session_header(LEGACY_SESSION_VERSION, [0xa5; PRIVACY_RESERVED_BYTES]);
        short_input.extend_from_slice(&frozen_record(
            1,
            0,
            1,
            INPUT_EVIDENCE_KIND,
            &7_u32.to_le_bytes(),
        ));
        assert_eq!(
            recover_session_bytes(&short_input),
            Err(StreamError::InvalidRecordPayload)
        );

        let mut long_end =
            frozen_session_header(LEGACY_SESSION_VERSION, [0xa5; PRIVACY_RESERVED_BYTES]);
        long_end.extend_from_slice(&frozen_record(
            1,
            0,
            0,
            END_KIND,
            &frozen_clean_end_payload(Some(true)),
        ));
        assert_eq!(
            recover_session_bytes(&long_end),
            Err(StreamError::InvalidRecordPayload)
        );
    }

    #[test]
    fn v3_session_rejects_retired_input_evidence_and_short_end() {
        let mut retired_input = frozen_session_header(SESSION_VERSION, [0; PRIVACY_RESERVED_BYTES]);
        let mut input_payload = 7_u32.to_le_bytes().to_vec();
        input_payload.extend_from_slice(&[0x5a; CHECKSUM_BYTES]);
        retired_input.extend_from_slice(&frozen_record(
            1,
            0,
            1,
            INPUT_EVIDENCE_KIND,
            &input_payload,
        ));
        assert_eq!(
            recover_session_bytes(&retired_input),
            Err(StreamError::InvalidRecordPayload)
        );

        let mut short_end = frozen_session_header(SESSION_VERSION, [0; PRIVACY_RESERVED_BYTES]);
        short_end.extend_from_slice(&frozen_record(
            1,
            0,
            0,
            END_KIND,
            &frozen_clean_end_payload(None),
        ));
        assert_eq!(
            recover_session_bytes(&short_end),
            Err(StreamError::InvalidRecordPayload)
        );
    }

    #[test]
    fn legacy_v1_record_decoder_rejects_v2_record_shapes() {
        let legacy_input = frozen_record(1, 0, 1, INPUT_EVIDENCE_KIND, &7_u32.to_le_bytes());
        assert_eq!(
            Record::decode(&legacy_input),
            Ok(Record {
                record_seq: 1,
                display_seq: 0,
                input_seq: 1,
                kind: RecordKind::InputEvidence {
                    input_seq: 1,
                    byte_len: 7,
                },
            })
        );
        assert_eq!(
            Record::decode(&{
                let mut retired_input = 7_u32.to_le_bytes().to_vec();
                retired_input.extend_from_slice(&[0x5a; CHECKSUM_BYTES]);
                frozen_record(1, 0, 1, INPUT_EVIDENCE_KIND, &retired_input)
            }),
            Err(StreamError::InvalidRecordPayload)
        );
        assert_eq!(
            Record::decode(&frozen_record(
                1,
                0,
                0,
                END_KIND,
                &frozen_clean_end_payload(None),
            )),
            Err(StreamError::InvalidRecordPayload)
        );
        assert_eq!(
            Record::decode(&frozen_record(1, 0, 0, END_KIND, &[])),
            Ok(Record {
                record_seq: 1,
                display_seq: 0,
                input_seq: 0,
                kind: RecordKind::LegacyEnd,
            })
        );
    }

    #[test]
    fn pty_stream_output_roundtrips_opaque_invalid_utf8() {
        let original = record(RecordKind::Output(vec![0xff, 0xfe, 0, b'x']));
        assert_eq!(Record::decode(&original.encode().unwrap()), Ok(original));
    }

    #[test]
    fn pty_stream_input_evidence_contains_only_sequence_and_length() {
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
        assert_eq!(
            &encoded[FRAME_LEN_BYTES + FRAME_FIXED_BYTES
                ..FRAME_LEN_BYTES + FRAME_FIXED_BYTES + size_of::<u32>()],
            &4_u32.to_le_bytes(),
            "InputEvidence contains only the accepted byte length after its ordering fields"
        );
    }

    #[test]
    fn pty_stream_rejects_unknown_kind_oversize_and_bad_checksum() {
        let original = record(RecordKind::LegacyEnd);
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
                    kind: RecordKind::LegacyEnd,
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
            kind: RecordKind::LegacyEnd,
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
        reported_len: Option<u64>,
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
            Ok(self.reported_len.unwrap_or(self.bytes.len() as u64))
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

    #[test]
    fn pty_stream_writer_recovery_rejects_oversized_storage_before_reading_it() {
        let mut writer = SegmentWriter::new(FakeStorage {
            reported_len: Some(MAX_RECOVERY_BYTES as u64 + 1),
            ..FakeStorage::default()
        })
        .unwrap();

        assert_eq!(
            writer.recover_and_repair(),
            Err(StreamError::RecoveryTooLarge {
                max: MAX_RECOVERY_BYTES
            })
        );
        assert!(writer.storage.events.is_empty());
        assert!(writer.storage.bytes.is_empty());
    }

    #[test]
    fn pty_stream_writer_rejects_aggregate_quota_before_encoding_or_writing() {
        let mut writer = SegmentWriter::new(FakeStorage {
            reported_len: Some(MAX_RECOVERY_BYTES as u64),
            ..FakeStorage::default()
        })
        .unwrap();

        assert_eq!(
            writer.commit(&mixed_segment()),
            Err(StreamError::RecoveryTooLarge {
                max: MAX_RECOVERY_BYTES
            })
        );
        assert!(writer.storage.events.is_empty());
        assert!(writer.storage.bytes.is_empty());
    }

    #[test]
    fn incremental_stream_reopen_rejects_an_oversized_sparse_file() {
        let dir = marion_testsupport::scratch("pty-stream-oversized-reopen");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let stream_path = stream_path_for_cast(&cast_path).unwrap();
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(stream_path)
            .unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .unwrap();
        file.set_len(MAX_RECOVERY_BYTES as u64 + 1).unwrap();

        assert!(matches!(
            SessionWriter::reopen(
                &cast_path,
                &AgentId("oversized-reopen".into()),
                super::super::WinSize::new(80, 24),
                [0; SESSION_ID_BYTES],
            ),
            Err(StreamError::RecoveryTooLarge {
                max: MAX_RECOVERY_BYTES
            })
        ));
    }

    #[test]
    fn incremental_stream_rejects_a_group_or_world_writable_parent() {
        let dir = marion_testsupport::scratch("pty-stream-unsafe-parent");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let cast_path = dir.join("pty.cast");
        let result = SessionWriter::create_with_session(
            &cast_path,
            &AgentId("unsafe-parent".into()),
            super::super::WinSize::new(80, 24),
            [0x41; SESSION_ID_BYTES],
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert!(!stream_path_for_cast(&cast_path).unwrap().exists());
    }

    #[test]
    fn incremental_stream_rejects_a_symlink_parent() {
        let dir = marion_testsupport::scratch("pty-stream-symlink-parent");
        let real = dir.join("real");
        let linked = dir.join("linked");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &linked).unwrap();
        let cast_path = linked.join("pty.cast");

        assert!(
            SessionWriter::create_with_session(
                &cast_path,
                &AgentId("symlink-parent".into()),
                super::super::WinSize::new(80, 24),
                [0x42; SESSION_ID_BYTES],
            )
            .is_err()
        );
        assert!(!real.join("pty.stream").exists());
    }

    #[test]
    fn incremental_stream_allows_only_one_live_writer() {
        let dir = marion_testsupport::scratch("pty-stream-single-writer");
        let cast_path = dir.join("pty.cast");
        let first = SessionWriter::create_with_session(
            &cast_path,
            &AgentId("single-writer".into()),
            super::super::WinSize::new(80, 24),
            [0x43; SESSION_ID_BYTES],
        )
        .unwrap();

        assert!(
            SessionWriter::reopen(
                &cast_path,
                &AgentId("single-writer".into()),
                super::super::WinSize::new(80, 24),
                [0x43; SESSION_ID_BYTES],
            )
            .is_err()
        );
        drop(first);
    }

    #[test]
    fn incremental_stream_is_private_and_cleans_up_a_failed_header() {
        let dir = marion_testsupport::scratch("pty-stream-private-cleanup");
        let cast_path = dir.join("pty.cast");
        let header = SessionHeader::new(
            &AgentId("private-cleanup".into()),
            super::super::WinSize::new(80, 24),
            [0x44; SESSION_ID_BYTES],
        );
        assert!(
            SessionWriter::create_prepared(&cast_path, header, |_file, _header| {
                Err(io::Error::other("injected header failure"))
            })
            .is_err()
        );
        assert!(!stream_path_for_cast(&cast_path).unwrap().exists());

        let writer = SessionWriter::create_with_session(
            &cast_path,
            &AgentId("private-cleanup".into()),
            super::super::WinSize::new(80, 24),
            [0x44; SESSION_ID_BYTES],
        )
        .unwrap();
        let mode = writer.file.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn incremental_stream_reopen_rejects_a_header_mismatch() {
        let dir = marion_testsupport::scratch("pty-stream-header-mismatch");
        let cast_path = dir.join("pty.cast");
        let writer = SessionWriter::create_with_session(
            &cast_path,
            &AgentId("expected-agent".into()),
            super::super::WinSize::new(80, 24),
            [0x45; SESSION_ID_BYTES],
        )
        .unwrap();
        drop(writer);

        assert_eq!(
            SessionWriter::reopen(
                &cast_path,
                &AgentId("wrong-agent".into()),
                super::super::WinSize::new(80, 24),
                [0x45; SESSION_ID_BYTES],
            )
            .err(),
            Some(StreamError::HeaderMismatch)
        );
    }

    #[test]
    fn incremental_stream_create_rejects_a_final_symlink() {
        let dir = marion_testsupport::scratch("pty-stream-final-symlink");
        let cast_path = dir.join("pty.cast");
        let target = dir.join("target");
        std::fs::write(&target, b"unchanged").unwrap();
        symlink(&target, stream_path_for_cast(&cast_path).unwrap()).unwrap();

        assert!(
            SessionWriter::create_with_session(
                &cast_path,
                &AgentId("final-symlink".into()),
                super::super::WinSize::new(80, 24),
                [0x46; SESSION_ID_BYTES],
            )
            .is_err()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
    }

    #[test]
    fn incremental_stream_rejects_oversized_output_without_growing_the_file() {
        let dir = marion_testsupport::scratch("pty-stream-oversized-output");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let agent = marion_core::contract::AgentId("oversized-output".into());
        let mut writer = SessionWriter::create_with_session(
            &cast_path,
            &agent,
            super::super::WinSize::new(80, 24),
            [0x22; SESSION_ID_BYTES],
        )
        .unwrap();
        let before = writer.file.metadata().unwrap().len();
        let oversized = vec![0; MAX_RECORD_BYTES - FRAME_FIXED_BYTES + 1];

        assert!(matches!(
            writer.append_output(&oversized),
            Err(StreamError::RecordTooLarge {
                max: MAX_RECORD_BYTES,
                ..
            })
        ));
        assert_eq!(writer.file.metadata().unwrap().len(), before);
    }

    #[test]
    fn incremental_stream_stops_output_at_its_reserved_control_boundary() {
        let dir = marion_testsupport::scratch("pty-stream-aggregate-quota");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let agent = marion_core::contract::AgentId("aggregate-quota".into());
        let mut writer = SessionWriter::create_with_session(
            &cast_path,
            &agent,
            super::super::WinSize::new(80, 24),
            [0x33; SESSION_ID_BYTES],
        )
        .unwrap();
        let payload = vec![0; MAX_RECORD_BYTES - FRAME_FIXED_BYTES];
        let encoded_frame_len = FRAME_LEN_BYTES + MAX_RECORD_BYTES + CHECKSUM_BYTES;
        let accepted = (SESSION_OUTPUT_LIMIT_BYTES - SESSION_HEADER_LEN) / encoded_frame_len;
        for _ in 0..accepted {
            writer.append_output(&payload).unwrap();
        }
        let before = writer.file.metadata().unwrap().len();

        assert_eq!(
            writer.append_output(&payload),
            Err(StreamError::OutputBudgetExhausted {
                max: SESSION_OUTPUT_LIMIT_BYTES
            })
        );
        let after = writer.file.metadata().unwrap().len();
        assert_eq!(
            after - before,
            encoded_record_len(0).unwrap() as u64,
            "the first quota crossing persists exactly one DisplayIncomplete control frame"
        );
        let recovery = recover_session_bytes(
            &std::fs::read(stream_path_for_cast(&cast_path).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            recovery.records.last().map(|record| &record.kind),
            Some(RecordKind::DisplayIncomplete)
        ));
    }

    /// Mutation: leave display truncation only in the caller's RAM after returning the quota
    /// error. Ignoring that error would then allow a falsely complete terminal record.
    #[test]
    fn session_seal_derives_incomplete_display_from_persisted_truncation() {
        let dir = marion_testsupport::scratch("pty-stream-persisted-truncation");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let agent = AgentId("persisted-truncation".into());
        let mut writer = SessionWriter::create_with_session(
            &cast_path,
            &agent,
            super::super::WinSize::new(80, 24),
            [0x35; SESSION_ID_BYTES],
        )
        .unwrap();
        writer.limit_output_for_test(SESSION_HEADER_LEN);

        let _ignored = writer.append_output(b"past the display quota");
        writer
            .seal(TerminalOutcome {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                reader: ReaderDisposition::CleanEof,
                cast_complete: true,
                stream_complete: true,
            })
            .unwrap();

        let recovered = recover_session_bytes(
            &std::fs::read(stream_path_for_cast(&cast_path).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            recovered.records.as_slice(),
            [
                Record {
                    kind: RecordKind::DisplayIncomplete,
                    ..
                },
                Record {
                    kind: RecordKind::End(TerminalOutcome {
                        stream_complete: false,
                        ..
                    }),
                    ..
                }
            ]
        ));
    }

    /// Mutation: derive truncation only from a live SessionWriter field. Reopen would lose the
    /// state and seal a truncated stream as replay-complete.
    #[test]
    fn session_reopen_recovers_persisted_truncation_before_seal() {
        let dir = marion_testsupport::scratch("pty-stream-reopen-truncation");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let agent = AgentId("reopen-truncation".into());
        let size = super::super::WinSize::new(80, 24);
        let session_id = [0x36; SESSION_ID_BYTES];
        let mut writer =
            SessionWriter::create_with_session(&cast_path, &agent, size, session_id).unwrap();
        writer.limit_output_for_test(SESSION_HEADER_LEN);
        assert!(matches!(
            writer.append_output(b"past the display quota"),
            Err(StreamError::OutputBudgetExhausted { .. })
        ));
        drop(writer);

        SessionWriter::reopen(&cast_path, &agent, size, session_id)
            .unwrap()
            .seal(TerminalOutcome {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                reader: ReaderDisposition::CleanEof,
                cast_complete: true,
                stream_complete: true,
            })
            .unwrap();

        let recovered = recover_session_bytes(
            &std::fs::read(stream_path_for_cast(&cast_path).unwrap()).unwrap(),
        )
        .unwrap();
        let RecordKind::End(outcome) = recovered.records.last().unwrap().kind else {
            panic!("sealed session ends with typed terminal evidence")
        };
        assert!(!outcome.stream_complete);
    }

    #[test]
    fn session_reopen_marks_a_discarded_partial_frame_as_display_incomplete() {
        let dir = marion_testsupport::scratch("pty-stream-torn-tail-truncation");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let agent = AgentId("torn-tail-truncation".into());
        let size = super::super::WinSize::new(80, 24);
        let session_id = [0x37; SESSION_ID_BYTES];
        let writer =
            SessionWriter::create_with_session(&cast_path, &agent, size, session_id).unwrap();
        drop(writer);
        let stream_path = stream_path_for_cast(&cast_path).unwrap();
        let torn_output = Record {
            record_seq: 1,
            display_seq: 1,
            input_seq: 0,
            kind: RecordKind::Output(b"possibly lost display".to_vec()),
        }
        .encode_for(RecordFormat::SessionV3)
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&stream_path)
            .unwrap();
        Write::write_all(&mut file, &torn_output[..torn_output.len() - 1]).unwrap();
        file.sync_data().unwrap();
        drop(file);

        SessionWriter::reopen(&cast_path, &agent, size, session_id)
            .unwrap()
            .seal(TerminalOutcome {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                reader: ReaderDisposition::CleanEof,
                cast_complete: true,
                stream_complete: true,
            })
            .unwrap();

        let recovered = recover_session_bytes(&std::fs::read(stream_path).unwrap()).unwrap();
        assert!(matches!(
            recovered.records.as_slice(),
            [
                Record {
                    kind: RecordKind::DisplayIncomplete,
                    ..
                },
                Record {
                    kind: RecordKind::End(TerminalOutcome {
                        stream_complete: false,
                        ..
                    }),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn failed_discontinuity_repairs_are_retried_on_the_next_reopen() {
        for failure in ["no-bytes", "partial", "full-before-sync"] {
            let dir = marion_testsupport::scratch(&format!("pty-stream-retry-repair-{failure}"));
            let cast_path = dir.join("pty.cast");
            std::fs::write(&cast_path, b"cast").unwrap();
            let agent = AgentId(format!("retry-repair-{failure}"));
            let size = super::super::WinSize::new(80, 24);
            let session_id = [0x38; SESSION_ID_BYTES];
            drop(SessionWriter::create_with_session(&cast_path, &agent, size, session_id).unwrap());

            let injection_entered = std::cell::Cell::new(false);
            let first = SessionWriter::reopen_prepared(
                &cast_path,
                &agent,
                size,
                session_id,
                |file, offset, marker| {
                    injection_entered.set(true);
                    file.seek(SeekFrom::Start(offset))?;
                    match failure {
                        "no-bytes" => {}
                        "partial" => Write::write_all(file, &marker[..8])?,
                        "full-before-sync" => Write::write_all(file, marker)?,
                        _ => unreachable!(),
                    }
                    Err(io::Error::other(format!(
                        "injected {failure} repair failure"
                    )))
                },
            );
            assert!(
                matches!(&first, Err(StreamError::Storage(error)) if error.contains(failure)),
                "{failure}: injection_entered={}; actual={:?}",
                injection_entered.get(),
                first.as_ref().err()
            );

            SessionWriter::reopen(&cast_path, &agent, size, session_id)
                .unwrap()
                .seal(TerminalOutcome {
                    exit_code: Some(0),
                    signal: None,
                    timed_out: false,
                    reader: ReaderDisposition::CleanEof,
                    cast_complete: true,
                    stream_complete: true,
                })
                .unwrap();
            let stream_path = stream_path_for_cast(&cast_path).unwrap();
            let recovered = recover_session_bytes(&std::fs::read(stream_path).unwrap()).unwrap();
            assert!(matches!(
                recovered.records.as_slice(),
                [
                    Record {
                        kind: RecordKind::DisplayIncomplete,
                        ..
                    },
                    Record {
                        kind: RecordKind::End(TerminalOutcome {
                            stream_complete: false,
                            ..
                        }),
                        ..
                    }
                ]
            ));
        }
    }

    /// Mutation: persist a keyed digest and its key beside low-entropy operator input evidence.
    /// A reader could then test guesses offline even though the raw input bytes were omitted.
    #[test]
    fn session_format_carries_only_content_independent_input_evidence() {
        let dir = marion_testsupport::scratch("pty-stream-input-evidence-privacy");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let agent = AgentId("input-evidence-privacy".into());
        let mut writer = SessionWriter::create_with_session(
            &cast_path,
            &agent,
            super::super::WinSize::new(80, 24),
            [0x41; SESSION_ID_BYTES],
        )
        .unwrap();

        writer.append_input_evidence(b"short secret".len()).unwrap();
        let encoded = std::fs::read(stream_path_for_cast(&cast_path).unwrap()).unwrap();
        let record_len = u32::from_le_bytes(
            encoded[SESSION_HEADER_LEN..SESSION_HEADER_LEN + FRAME_LEN_BYTES]
                .try_into()
                .unwrap(),
        ) as usize;

        assert_eq!(
            record_len,
            FRAME_FIXED_BYTES + size_of::<u32>(),
            "input evidence may retain byte length and sequence placement, but no content-verifiable digest"
        );
        assert_eq!(
            &encoded[SESSION_HEADER_PREFIX_LEN - PRIVACY_RESERVED_BYTES..SESSION_HEADER_PREFIX_LEN],
            &[0; PRIVACY_RESERVED_BYTES],
            "the session header must not persist a key that makes input guesses verifiable"
        );
    }

    #[test]
    fn incremental_stream_binds_session_and_orders_content_independent_input_evidence() {
        let dir = marion_testsupport::scratch("pty-stream-incremental-binding");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let agent = marion_core::contract::AgentId("agent-bound-to-stream".into());
        let session_id = [0x11; SESSION_ID_BYTES];
        let raw_input = [0x00, 0xff, 0x80, b'x'];

        let mut writer = SessionWriter::create_with_session(
            &cast_path,
            &agent,
            super::super::WinSize::new(80, 24),
            session_id,
        )
        .unwrap();
        writer.append_output(&[0xff, b'o']).unwrap();
        writer
            .append_resize(super::super::WinSize::new(100, 31))
            .unwrap();
        writer.append_input_evidence(raw_input.len()).unwrap();
        let outcome = TerminalOutcome {
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            reader: ReaderDisposition::CleanEof,
            cast_complete: true,
            stream_complete: true,
        };
        writer.seal(outcome).unwrap();

        let stream_path = stream_path_for_cast(&cast_path).unwrap();
        let encoded = std::fs::read(&stream_path).unwrap();
        let recovered = recover_session_bytes(&encoded).unwrap();
        assert_eq!(recovered.header.session_id, session_id);
        assert_eq!(recovered.header.initial_rows, 24);
        assert_eq!(recovered.header.initial_cols, 80);
        assert_eq!(recovered.records.len(), 4);
        assert_eq!(recovered.records[0].record_seq, 1);
        assert_eq!(recovered.records[0].display_seq, 1);
        assert_eq!(recovered.records[1].record_seq, 2);
        assert_eq!(recovered.records[1].display_seq, 2);
        assert_eq!(recovered.records[2].record_seq, 3);
        assert_eq!(recovered.records[2].display_seq, 2);
        assert_eq!(recovered.records[2].input_seq, 1);
        let RecordKind::InputEvidence {
            input_seq,
            byte_len,
        } = &recovered.records[2].kind
        else {
            panic!("the third incremental record is input evidence")
        };
        assert_eq!(*input_seq, 1);
        assert_eq!(*byte_len, raw_input.len() as u32);
        assert_eq!(recovered.records[3].record_seq, 4);
        assert_eq!(recovered.records[3].kind, RecordKind::End(outcome));
        assert_eq!(recovered.truncate_to, None);
    }

    #[test]
    fn session_v3_header_has_no_content_verification_key() {
        let header = SessionHeader::new(
            &AgentId("content-independent-input".into()),
            super::super::WinSize::new(80, 24),
            [0x51; SESSION_ID_BYTES],
        );

        assert_eq!(header.version, 3);
        assert_eq!(header.privacy_reserved, [0; PRIVACY_RESERVED_BYTES]);
        assert_eq!(SessionHeader::decode(&header.encode()), Ok(header));
    }

    #[test]
    fn terminal_outcome_validation_and_replay_eligibility_are_explicit() {
        let clean = TerminalOutcome {
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            reader: ReaderDisposition::CleanEof,
            cast_complete: true,
            stream_complete: true,
        };
        assert_eq!(clean.validate(), Ok(()));
        assert!(clean.replay_eligible());

        let forced = TerminalOutcome {
            reader: ReaderDisposition::ForcedStop,
            ..clean
        };
        assert_eq!(forced.validate(), Ok(()));
        assert!(!forced.replay_eligible());
        assert!(
            !TerminalOutcome {
                cast_complete: false,
                ..clean
            }
            .replay_eligible()
        );
        assert!(
            !TerminalOutcome {
                stream_complete: false,
                ..clean
            }
            .replay_eligible()
        );

        assert_eq!(
            TerminalOutcome {
                exit_code: Some(1),
                signal: Some(9),
                ..clean
            }
            .validate(),
            Err(StreamError::InvalidTerminalOutcome)
        );
        assert_eq!(
            TerminalOutcome {
                exit_code: None,
                signal: None,
                ..clean
            }
            .validate(),
            Err(StreamError::InvalidTerminalOutcome)
        );
        assert_eq!(
            TerminalOutcome {
                exit_code: None,
                signal: None,
                timed_out: true,
                ..clean
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            TerminalOutcome {
                exit_code: Some(1),
                signal: Some(9),
                timed_out: true,
                ..clean
            }
            .validate(),
            Err(StreamError::InvalidTerminalOutcome)
        );
        assert!(
            !TerminalOutcome {
                exit_code: Some(1),
                signal: Some(9),
                ..clean
            }
            .replay_eligible()
        );
    }

    #[test]
    fn versioned_session_and_legacy_segment_reject_each_others_end_representation() {
        let header = SessionHeader::new(
            &AgentId("cross-version-end".into()),
            super::super::WinSize::new(80, 24),
            [0x81; SESSION_ID_BYTES],
        );
        let mut session_bytes = header.encode().to_vec();
        session_bytes.extend_from_slice(
            &Record {
                record_seq: 1,
                display_seq: 0,
                input_seq: 0,
                kind: RecordKind::LegacyEnd,
            }
            .encode()
            .unwrap(),
        );
        assert_eq!(
            recover_session_bytes(&session_bytes),
            Err(StreamError::MissingTerminalOutcome)
        );

        let outcome = TerminalOutcome {
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            reader: ReaderDisposition::CleanEof,
            cast_complete: true,
            stream_complete: true,
        };
        assert_eq!(
            Segment {
                records: vec![Record {
                    record_seq: 1,
                    display_seq: 0,
                    input_seq: 0,
                    kind: RecordKind::End(outcome),
                }],
            }
            .encode(),
            Err(StreamError::TypedTerminalOutcomeInLegacySegment)
        );
    }

    #[test]
    fn sealing_commits_one_typed_end_and_reopen_stays_sealed() {
        let dir = marion_testsupport::scratch("pty-stream-typed-seal");
        let cast_path = dir.join("pty.cast");
        std::fs::write(&cast_path, b"cast").unwrap();
        let agent = AgentId("typed-seal".into());
        let size = super::super::WinSize::new(80, 24);
        let session_id = [0x71; SESSION_ID_BYTES];
        let outcome = TerminalOutcome {
            exit_code: None,
            signal: Some(15),
            timed_out: true,
            reader: ReaderDisposition::CleanEof,
            cast_complete: true,
            stream_complete: true,
        };

        let mut writer =
            SessionWriter::create_with_session(&cast_path, &agent, size, session_id).unwrap();
        writer.append_output(b"tail").unwrap();
        writer.seal(outcome).unwrap();

        let encoded = std::fs::read(stream_path_for_cast(&cast_path).unwrap()).unwrap();
        let recovered = recover_session_bytes(&encoded).unwrap();
        assert_eq!(
            recovered.records.last().map(|record| &record.kind),
            Some(&RecordKind::End(outcome))
        );

        let mut reopened = SessionWriter::reopen(&cast_path, &agent, size, session_id).unwrap();
        assert_eq!(
            reopened.append_output(b"after end"),
            Err(StreamError::SessionEnded)
        );
    }
}
