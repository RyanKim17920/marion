//! The verbatim request log.
//!
//! M1's third acceptance criterion is asserted against this file, so it is written to be *read by a
//! program*: one JSON object per line, appended under a lock, flushed before the response goes out.
//! An unparseable body is still logged (as `body_raw`) rather than dropped — a request we could not
//! understand is exactly the request an investigation will want.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

/// Header names whose values are credentials. Recorded as present, never verbatim.
///
/// `x-goog-api-key` is the Gemini wire's key header (S12) — a different spelling of the same
/// secret, and one this list would have missed simply because it was written before that wire
/// existed. A new wire is a reason to re-read this array, not only to extend `Wire`.
const REDACTED: [&str; 5] = [
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "proxy-authorization",
    "cookie",
];

/// An append-only JSONL sink, safe to share across connection threads.
#[derive(Debug)]
pub struct RequestLog {
    path: PathBuf,
    file: Mutex<File>,
    seq: AtomicU64,
}

impl RequestLog {
    /// Open (creating, appending) the log at `path`.
    ///
    /// Owner-only, `0700` directories and a `0600` file: a request body is the node's prompt and
    /// history, which no other local user should be able to read.
    pub fn create(path: impl Into<PathBuf>) -> io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        let path = path.into();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            seq: AtomicU64::new(0),
        })
    }

    /// Where the log lives, so a caller can hand the path to an assertion or a child process.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record. `seq` is arrival order — recorded because it is useful evidence, and
    /// deliberately not consulted by any dispatch decision.
    pub fn append(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
        wire: Option<&str>,
    ) -> io::Result<u64> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut record = json!({
            "seq": seq,
            "method": method,
            "path": path,
            "wire": wire,
            "headers": headers_value(headers),
        });
        let credentials = credentials_value(headers);
        if credentials.as_object().is_some_and(|m| !m.is_empty()) {
            record["credentials"] = credentials;
        }
        match serde_json::from_slice::<Value>(body) {
            Ok(v) => record["body"] = v,
            Err(e) => {
                record["body"] = Value::Null;
                record["body_raw"] = json!(String::from_utf8_lossy(body));
                record["body_parse_error"] = json!(e.to_string());
            }
        }
        let mut line = serde_json::to_string(&record).expect("a Value always serialises");
        line.push('\n');
        let mut file = self.file.lock().expect("request log mutex poisoned");
        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(seq)
    }

    /// Read the log back as parsed records. For tests and for M1's acceptance assertions.
    pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<Value>> {
        let text = std::fs::read_to_string(path)?;
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(io::Error::other))
            .collect()
    }
}

/// A fingerprint of a credential header's value: FNV-1a over its bytes, as `fnv1a64:<hex>`.
///
/// Evidence of **which** credential arrived — a test compares it against the fingerprint of its
/// own fixture key — without the log holding the credential. Not a cryptographic hash, and it need
/// not be: the canned server only ever sees fixture keys, and the value is never a lookup key.
pub fn fingerprint(value: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in value.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{h:016x}")
}

/// The redacted headers' [`fingerprint`]s, by lowercased name.
fn credentials_value(headers: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        let key = name.to_ascii_lowercase();
        if REDACTED.contains(&key.as_str()) {
            map.insert(key, Value::String(fingerprint(value)));
        }
    }
    Value::Object(map)
}

fn headers_value(headers: &[(String, String)]) -> Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        let key = name.to_ascii_lowercase();
        let value = if REDACTED.contains(&key.as_str()) {
            "<redacted>".to_string()
        } else {
            value.clone()
        };
        map.insert(key, Value::String(value));
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("marion-reqlog-{}-{name}.jsonl", std::process::id()))
    }

    /// Mutation: open without `mode` and the log reads `0644`.
    #[test]
    fn the_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("private");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        RequestLog::create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_file(&path);
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_record_round_trips_with_its_body_parsed() {
        let path = tmp("roundtrip");
        let _ = std::fs::remove_file(&path);
        let log = RequestLog::create(&path).unwrap();
        let headers = vec![("Content-Type".into(), "application/json".into())];
        log.append(
            "POST",
            "/v1/responses",
            &headers,
            br#"{"input":[]}"#,
            Some("responses"),
        )
        .unwrap();

        let back = RequestLog::read(&path).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0]["path"], "/v1/responses");
        assert_eq!(back[0]["wire"], "responses");
        assert!(
            back[0]["body"]["input"].is_array(),
            "the body is stored parsed, not as a string"
        );
        assert_eq!(back[0]["headers"]["content-type"], "application/json");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_gemini_and_openai_wires_round_trip_with_their_own_shapes() {
        let path = tmp("new-wires");
        let _ = std::fs::remove_file(&path);
        let log = RequestLog::create(&path).unwrap();
        log.append(
            "POST",
            "/v1beta/models/gemini-3.5-flash:streamGenerateContent?alt=sse",
            &[("x-goog-api-client".into(), "google-genai-sdk/1.30.0".into())],
            br#"{"contents":[{"role":"user","parts":[{"text":"go"}]}]}"#,
            Some("gemini"),
        )
        .unwrap();
        log.append(
            "POST",
            "/v1/chat/completions",
            &[("Authorization".into(), "Bearer sk-fake".into())],
            br#"{"model":"fake-1","messages":[],"stream":true}"#,
            Some("openai"),
        )
        .unwrap();

        let back = RequestLog::read(&path).unwrap();
        assert_eq!(back[0]["wire"], "gemini");
        assert!(back[0]["body"]["contents"].is_array());
        assert_eq!(
            back[0]["headers"]["x-goog-api-client"], "google-genai-sdk/1.30.0",
            "the SDK header is evidence of which client spoke and is not a credential"
        );
        assert_eq!(back[1]["wire"], "openai");
        assert!(back[1]["body"]["messages"].is_array());
        assert_eq!(
            back[1]["headers"]["authorization"], "<redacted>",
            "opencode sends its key as a bearer token"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// **Which credential arrived is evidence; the credential is not.** An endpoint test must be
    /// able to say "this request presented `Bearer sk-endpoint-test`" without the log holding it,
    /// so each redacted header also records a fingerprint the test can compute from its own
    /// fixture key.
    #[test]
    fn a_redacted_credential_is_recorded_by_fingerprint() {
        let path = tmp("fingerprint");
        let _ = std::fs::remove_file(&path);
        let log = RequestLog::create(&path).unwrap();
        let headers = vec![("Authorization".into(), "Bearer sk-endpoint-test".into())];
        log.append("POST", "/v1/chat/completions", &headers, b"{}", None)
            .unwrap();
        let back = RequestLog::read(&path).unwrap();
        assert_eq!(back[0]["headers"]["authorization"], "<redacted>");
        assert_eq!(
            back[0]["credentials"]["authorization"],
            fingerprint("Bearer sk-endpoint-test")
        );
        assert_ne!(fingerprint("Bearer a"), fingerprint("Bearer b"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-endpoint-test"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn credentials_are_recorded_as_present_but_not_verbatim() {
        let path = tmp("redact");
        let _ = std::fs::remove_file(&path);
        let log = RequestLog::create(&path).unwrap();
        let headers = vec![("Authorization".into(), "Bearer sk-live-secret".into())];
        log.append("POST", "/v1/messages", &headers, b"{}", None)
            .unwrap();
        // Every wire spells the key header differently; each spelling is the same secret.
        let goog = vec![("X-Goog-Api-Key".into(), "AIza-live-secret".into())];
        log.append(
            "POST",
            "/v1beta/models/m:generateContent",
            &goog,
            b"{}",
            None,
        )
        .unwrap();

        let back = RequestLog::read(&path).unwrap();
        assert_eq!(back[0]["headers"]["authorization"], "<redacted>");
        assert_eq!(back[1]["headers"]["x-goog-api-key"], "<redacted>");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-live-secret"));
        assert!(!text.contains("AIza-live-secret"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unparseable_body_is_kept_rather_than_dropped() {
        let path = tmp("unparseable");
        let _ = std::fs::remove_file(&path);
        let log = RequestLog::create(&path).unwrap();
        log.append("POST", "/v1/messages", &[], b"not json", None)
            .unwrap();

        let back = RequestLog::read(&path).unwrap();
        assert_eq!(back[0]["body"], Value::Null);
        assert_eq!(back[0]["body_raw"], "not json");
        assert!(back[0]["body_parse_error"].is_string());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seq_counts_arrivals_so_a_reader_can_see_the_title_race() {
        let path = tmp("seq");
        let _ = std::fs::remove_file(&path);
        let log = RequestLog::create(&path).unwrap();
        log.append("POST", "/a", &[], b"{}", None).unwrap();
        log.append("POST", "/b", &[], b"{}", None).unwrap();
        let back = RequestLog::read(&path).unwrap();
        assert_eq!(
            (back[0]["seq"].as_u64(), back[1]["seq"].as_u64()),
            (Some(1), Some(2))
        );
        let _ = std::fs::remove_file(&path);
    }
}
