//! Durable binary PTY stream primitives.

// This increment deliberately lands the format and durability machinery before its production
// callsite, so every item in this module is expected to remain dark until that integration lands.
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use marion_core::contract::AgentId;
use thiserror::Error;

const FRAME_LEN_BYTES: usize = size_of::<u32>();
const CHECKSUM_BYTES: usize = 32;
const FRAME_FIXED_BYTES: usize = 1 + (3 * size_of::<u64>()) + size_of::<u32>();
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_RECOVERY_BYTES: usize = 16 * 1024 * 1024;
/// Leave room after display capture stops for ordered input evidence, geometry, and terminal End.
/// Exhausting this display budget is an honest truncation boundary, not a writer failure.
const SESSION_CONTROL_RESERVE_BYTES: usize = 1024 * 1024;
const SESSION_OUTPUT_LIMIT_BYTES: usize = MAX_RECOVERY_BYTES - SESSION_CONTROL_RESERVE_BYTES;

const OUTPUT_KIND: u8 = 1;
const RESIZE_KIND: u8 = 2;
const INPUT_EVIDENCE_KIND: u8 = 3;
const END_KIND: u8 = 4;
const DISPLAY_INCOMPLETE_KIND: u8 = 5;

const SESSION_MAGIC: [u8; 8] = *b"MRNPTS01";
const SESSION_VERSION: u16 = 3;
const SESSION_ID_BYTES: usize = 16;
const AGENT_BINDING_BYTES: usize = 32;
/// Reserved zero bytes where v2 persisted a content-verification key. Writers emit zeros, and a
/// reader refuses anything else rather than hold a key that would make input guesses verifiable.
const PRIVACY_RESERVED_BYTES: usize = 32;
const SESSION_HEADER_PREFIX_LEN: usize = SESSION_MAGIC.len()
    + size_of::<u16>()
    + (2 * size_of::<u16>())
    + SESSION_ID_BYTES
    + AGENT_BINDING_BYTES
    + PRIVACY_RESERVED_BYTES;
const SESSION_HEADER_LEN: usize = SESSION_HEADER_PREFIX_LEN + CHECKSUM_BYTES;
const AGENT_BINDING_DOMAIN: &[u8] = b"marion.pty.agent-binding.v1\0";
const TERMINAL_OUTCOME_BYTES: usize = 12;
const INPUT_EVIDENCE_PAYLOAD_BYTES: usize = size_of::<u32>();
const EXIT_CODE_PRESENT: u8 = 1 << 0;
const SIGNAL_PRESENT: u8 = 1 << 1;
const TIMED_OUT: u8 = 1 << 2;

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
    #[error("PTY stream record sequence is not dense: expected {expected}, got {actual}")]
    RecordSequenceGap { expected: u64, actual: u64 },
    #[error("PTY stream display sequence is invalid: expected {expected}, got {actual}")]
    DisplaySequenceGap { expected: u64, actual: u64 },
    #[error("PTY stream input sequence is invalid: expected {expected}, got {actual}")]
    InputSequenceGap { expected: u64, actual: u64 },
    #[error("PTY stream record appears after End")]
    RecordAfterEnd,
    #[error("PTY stream recovery input exceeds maximum {max} bytes")]
    RecoveryTooLarge { max: usize },
    #[error("PTY stream display output reached its bounded {max}-byte budget")]
    OutputBudgetExhausted { max: usize },
    #[error("PTY stream committed length overflow")]
    CommittedLengthOverflow,
    #[error("PTY stream writer is poisoned after a prior durability failure")]
    WriterPoisoned,
    #[error("PTY stream session id entropy failed: {0}")]
    Entropy(String),
    #[error("PTY stream session header checksum mismatch")]
    HeaderChecksumMismatch,
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

    fn decode(encoded: &[u8]) -> Result<Self, StreamError> {
        if encoded.len() != TERMINAL_OUTCOME_BYTES
            || encoded[0] & !(EXIT_CODE_PRESENT | SIGNAL_PRESENT | TIMED_OUT) != 0
            || encoded[10] > 1
            || encoded[11] > 1
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
            stream_complete: encoded[11] != 0,
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
    /// The session terminator, including the lifecycle evidence needed to judge replayability.
    End(TerminalOutcome),
}

impl Record {
    fn encoded_len(&self) -> Result<usize, StreamError> {
        let payload_len = match &self.kind {
            RecordKind::Output(bytes) => bytes.len(),
            RecordKind::Resize { .. } => 2 * size_of::<u16>(),
            RecordKind::InputEvidence { input_seq, .. } => {
                self.check_input_seq(*input_seq)?;
                INPUT_EVIDENCE_PAYLOAD_BYTES
            }
            RecordKind::DisplayIncomplete => 0,
            RecordKind::End(outcome) => {
                outcome.validate()?;
                TERMINAL_OUTCOME_BYTES
            }
        };
        encoded_record_len(payload_len)
    }

    /// An input-evidence record's payload must repeat the header's input sequence.
    fn check_input_seq(&self, input_seq: u64) -> Result<(), StreamError> {
        if input_seq != self.input_seq {
            return Err(StreamError::InputSequenceMismatch);
        }
        Ok(())
    }

    /// `u32 frame_len` (little-endian), then record bytes, followed by a BLAKE3 checksum of those
    /// record bytes. The record itself uses only explicit little-endian integer encodings.
    fn encode(&self) -> Result<Vec<u8>, StreamError> {
        let encoded_len = self.encoded_len()?;
        let (kind, payload) = self.encode_payload()?;

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

    /// The kind byte and payload bytes this record's kind writes.
    fn encode_payload(&self) -> Result<(u8, Vec<u8>), StreamError> {
        Ok(match &self.kind {
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
                self.check_input_seq(*input_seq)?;
                (INPUT_EVIDENCE_KIND, byte_len.to_le_bytes().to_vec())
            }
            RecordKind::DisplayIncomplete => (DISPLAY_INCOMPLETE_KIND, Vec::new()),
            RecordKind::End(outcome) => (END_KIND, outcome.encode()?.to_vec()),
        })
    }

    fn decode(encoded: &[u8]) -> Result<Self, StreamError> {
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
            kind: decode_record_kind(kind, payload, input_seq)?,
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

/// Interpret a checked record's kind byte and payload.
fn decode_record_kind(kind: u8, payload: &[u8], input_seq: u64) -> Result<RecordKind, StreamError> {
    match kind {
        OUTPUT_KIND => Ok(RecordKind::Output(payload.to_vec())),
        RESIZE_KIND => decode_resize_payload(payload),
        INPUT_EVIDENCE_KIND => decode_input_evidence_payload(payload, input_seq),
        DISPLAY_INCOMPLETE_KIND if payload.is_empty() => Ok(RecordKind::DisplayIncomplete),
        DISPLAY_INCOMPLETE_KIND => Err(StreamError::InvalidRecordPayload),
        END_KIND => decode_end_payload(payload),
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

/// Input evidence persists only the accepted byte length; its bytes are never stored.
fn decode_input_evidence_payload(
    payload: &[u8],
    input_seq: u64,
) -> Result<RecordKind, StreamError> {
    let byte_len = payload
        .try_into()
        .map_err(|_| StreamError::InvalidRecordPayload)?;
    Ok(RecordKind::InputEvidence {
        input_seq,
        byte_len: u32::from_le_bytes(byte_len),
    })
}

fn decode_end_payload(payload: &[u8]) -> Result<RecordKind, StreamError> {
    if payload.is_empty() {
        return Err(StreamError::MissingTerminalOutcome);
    }
    if payload.len() != TERMINAL_OUTCOME_BYTES {
        return Err(StreamError::InvalidRecordPayload);
    }
    Ok(RecordKind::End(TerminalOutcome::decode(payload)?))
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

/// Where the record, display and input sequences stand after the last committed record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrailerCounters {
    record_seq: u64,
    display_seq: u64,
    input_seq: u64,
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
        if version != SESSION_VERSION {
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
        if encoded_reserved != [0; PRIVACY_RESERVED_BYTES] {
            return Err(StreamError::HeaderMismatch);
        }
        Ok(Self {
            version,
            initial_cols,
            initial_rows,
            session_id,
            agent_binding,
            privacy_reserved: encoded_reserved,
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

/// Take the stream's exclusive writer lock without waiting.
///
/// The stream is created with `O_EXCL`, so no other writer can already hold this inode's lock; a
/// refusal is a real conflict, never a stale lock to wait out.
fn lock_writer(file: &File) -> Result<(), StreamError> {
    match rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::AGAIN => Err(StreamError::WriterLocked),
        Err(error) => Err(StreamError::Storage(error.to_string())),
    }
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
        let record = Record::decode(&encoded[offset..frame_end])?;
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
    validate_record_sequence(record, counters)?;
    validate_display_completeness(record, display_incomplete)?;
    counters.record_seq = record.record_seq;
    counters.display_seq = record.display_seq;
    counters.input_seq = record.input_seq;
    Ok(())
}

/// The three counters advance densely, and an input record's own sequence is the one it was placed
/// at.
///
/// Which counters a record advances is a property of its kind: output and resize advance the
/// display axis, input evidence the input axis, and every record advances the record axis. A gap on
/// any of them is a record that was lost between two that survived, which is what recovery exists to
/// refuse rather than to paper over.
fn validate_record_sequence(
    record: &Record,
    counters: &TrailerCounters,
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
    Ok(())
}

/// Display completeness is a one-way latch, and these are the three ways a stream could contradict
/// it: display output after the marker, the marker stated twice, and an End claiming the replay is
/// complete when the display was already known not to be.
fn validate_display_completeness(
    record: &Record,
    display_incomplete: &mut bool,
) -> Result<(), StreamError> {
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
    Ok(())
}

pub(crate) struct SessionWriter {
    file: File,
    counters: TrailerCounters,
    committed_len: u64,
    output_limit: u64,
    display_incomplete: bool,
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
            counters: TrailerCounters {
                record_seq: 0,
                display_seq: 0,
                input_seq: 0,
            },
            committed_len: SESSION_HEADER_LEN as u64,
            output_limit: SESSION_OUTPUT_LIMIT_BYTES as u64,
            display_incomplete: false,
            poisoned: false,
        })
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
        let record = self.next_record(kind)?;
        let record_seq = record.record_seq;
        let display_seq = record.display_seq;
        let input_seq = record.input_seq;
        let encoded_len = record.encoded_len()?;
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
        let encoded = record.encode()?;
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
        u64::try_from(record.encoded_len()?).map_err(|_| StreamError::CommittedLengthOverflow)
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

    /// A clean End outcome missing its final `stream_complete` byte, as v2 wrote it.
    fn frozen_short_end_payload() -> Vec<u8> {
        let mut payload = vec![0; TERMINAL_OUTCOME_BYTES - 1];
        payload[0] = EXIT_CODE_PRESENT;
        payload[9] = ReaderDisposition::CleanEof.encode();
        payload[10] = 1;
        payload
    }

    #[test]
    fn a_session_header_is_only_ever_v3_with_a_zeroed_reserve() {
        assert_eq!(
            recover_session_bytes(&frozen_session_header(2, [0; PRIVACY_RESERVED_BYTES])),
            Err(StreamError::UnsupportedVersion(2))
        );
        assert_eq!(
            recover_session_bytes(&frozen_session_header(
                SESSION_VERSION,
                [0xa5; PRIVACY_RESERVED_BYTES]
            )),
            Err(StreamError::HeaderMismatch)
        );
        assert!(
            recover_session_bytes(&frozen_session_header(
                SESSION_VERSION,
                [0; PRIVACY_RESERVED_BYTES]
            ))
            .unwrap()
            .records
            .is_empty()
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
            &frozen_short_end_payload(),
        ));
        assert_eq!(
            recover_session_bytes(&short_end),
            Err(StreamError::InvalidRecordPayload)
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
        let original = record(RecordKind::Resize { rows: 24, cols: 80 });
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
            SessionWriter::create_with_session(
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
    fn a_session_end_without_a_typed_outcome_is_refused() {
        let header = SessionHeader::new(
            &AgentId("untyped-end".into()),
            super::super::WinSize::new(80, 24),
            [0x81; SESSION_ID_BYTES],
        );
        let mut session_bytes = header.encode().to_vec();
        session_bytes.extend_from_slice(&frozen_record(1, 0, 0, END_KIND, &[]));
        assert_eq!(
            recover_session_bytes(&session_bytes),
            Err(StreamError::MissingTerminalOutcome)
        );
    }

    #[test]
    fn sealing_commits_one_typed_end() {
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
    }
}
