//! **One request to the provider, through `curl`** — the way `marion doctor --providers` reaches a
//! provider, and for the same reasons: marion links no TLS stack, and the key must reach neither
//! argv nor a file.
//!
//! The key rides curl's `-H @/dev/fd/3`: a pipe marion writes the header lines into and closes
//! before curl starts, inherited as descriptor 3 and read by curl as a header file. The body rides
//! stdin (`--data-binary @-`). curl's argv carries only the URL and fixed flags; `-q` (first, as curl
//! requires) keeps an operator's `~/.curlrc` from adding anything to the request. `-i` puts the
//! response head on stdout ahead of the body and `-N` turns off curl's output buffering, so the
//! provider's stream reaches the harness event by event.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};

use marion_core::provider::KeyHeader;

use crate::credentials::Secret;

/// The descriptor curl reads the header lines from.
const HEADER_FD: i32 = 3;

/// How a request authenticates to the provider: the key and the header it rides, or nothing for a
/// provider that authenticates nothing.
#[derive(Debug, Clone)]
pub struct Auth {
    pub key: Option<Secret>,
    pub header: KeyHeader,
    /// Header lines the provider's wire requires beside the key (`anthropic-version`).
    pub extra: &'static [&'static str],
}

/// The header lines curl is handed on descriptor 3.
fn header_lines(auth: &Auth) -> String {
    let mut lines = String::from("Content-Type: application/json\nAccept: text/event-stream\n");
    for line in auth.extra {
        lines.push_str(line);
        lines.push('\n');
    }
    if let Some(k) = &auth.key {
        match auth.header {
            KeyHeader::Bearer => lines.push_str("Authorization: Bearer "),
            KeyHeader::XApiKey => lines.push_str("x-api-key: "),
        }
        lines.push_str(k.expose());
        lines.push('\n');
    }
    lines
}

/// The provider's answer: its status and the headers the gateway reads, and the body as it arrives.
pub struct Answer {
    pub status: u16,
    pub content_type: String,
    pub retry_after: Option<String>,
    pub body: BufReader<ChildStdout>,
}

/// Start curl for `url`, hand it the header lines and the body, and read the response head.
///
/// **The child is handed to `hold` before a byte is written to it**, its pipes already taken, so
/// its owner can kill it (the gateway, stopping) while this blocks on the write or the head —
/// the kill closes both pipes and ends the wait — and must reap it.
pub fn post(
    url: &str,
    auth: &Auth,
    body: &[u8],
    hold: impl FnOnce(Child),
) -> io::Result<Result<Answer, String>> {
    let (header_r, mut header_w) = std::io::pipe()?;
    // A few hundred bytes: the pipe holds them, so writing before curl exists cannot block.
    header_w.write_all(header_lines(auth).as_bytes())?;
    drop(header_w);
    let header_r: OwnedFd = header_r.into();
    let mut cmd = Command::new("curl");
    cmd.args([
        "-q",
        "-sS",
        "-N",
        "-i",
        "--connect-timeout",
        "30",
        "-X",
        "POST",
        "-H",
        "@/dev/fd/3",
        "--data-binary",
        "@-",
        "--",
        url,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let raw = header_r.as_raw_fd();
    // SAFETY: async-signal-safe body only — `dup2`/`fcntl`, no allocation, no locks. `header_r` is
    // owned by this frame until `spawn` returns, so the descriptor is open in the forked child.
    unsafe {
        cmd.pre_exec(move || {
            let mut target = OwnedFd::from_raw_fd(HEADER_FD);
            let r = if raw == HEADER_FD {
                // Already on 3: only its close-on-exec flag has to go.
                rustix::io::fcntl_setfd(&target, rustix::io::FdFlags::empty())
            } else {
                rustix::io::dup2(BorrowedFd::borrow_raw(raw), &mut target)
            };
            // Neither handle is ours to close in the child: `exec` follows.
            std::mem::forget(target);
            r.map_err(io::Error::from)
        });
    }
    let mut child = cmd.spawn()?;
    drop(header_r);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take();
    hold(child);
    if let Some(mut stdin) = stdin {
        // curl reads all of stdin before it sends, so this write completes once curl is reading;
        // an error means curl already exited, which its stderr explains below.
        let _ = stdin.write_all(body);
    }
    Ok(match read_head(BufReader::new(stdout)) {
        Ok(a) => Ok(a),
        Err(_) => Err(curl_words(stderr)),
    })
}

/// What curl said on stderr, where it could not produce a response at all.
fn curl_words(stderr: Option<ChildStderr>) -> String {
    let mut words = String::new();
    if let Some(mut e) = stderr {
        let _ = e.by_ref().take(4096).read_to_string(&mut words);
    }
    let words = words.trim();
    if words.is_empty() {
        "curl produced no response".to_string()
    } else {
        words.to_string()
    }
}

/// Read the response head `-i` put ahead of the body: past any interim head (a `100 Continue`, a
/// proxy's `200 Connection established`) to the final one.
fn read_head(mut r: BufReader<ChildStdout>) -> io::Result<Answer> {
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no status line"))?;
        let (mut content_type, mut retry_after) = (String::new(), None);
        loop {
            let mut h = String::new();
            if r.read_line(&mut h)? == 0 {
                break;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "content-type" => content_type = v.trim().to_string(),
                    "retry-after" => retry_after = Some(v.trim().to_string()),
                    _ => {}
                }
            }
        }
        let interim = (100..200).contains(&status) || r.fill_buf()?.starts_with(b"HTTP/");
        if !interim {
            return Ok(Answer {
                status,
                content_type,
                retry_after,
                body: r,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_lines_carry_the_key_in_the_providers_header_and_nothing_without_one() {
        let key = Some(Secret::new("sk-header-test-1"));
        let bearer = header_lines(&Auth {
            key: key.clone(),
            header: KeyHeader::Bearer,
            extra: &[],
        });
        assert!(bearer.contains("\nAuthorization: Bearer sk-header-test-1\n"));
        let xkey = header_lines(&Auth {
            key,
            header: KeyHeader::XApiKey,
            extra: &["anthropic-version: 2023-06-01"],
        });
        assert!(xkey.contains("\nanthropic-version: 2023-06-01\n"));
        assert!(xkey.contains("\nx-api-key: sk-header-test-1\n") && !xkey.contains("Bearer"));
        let none = header_lines(&Auth {
            key: None,
            header: KeyHeader::Bearer,
            extra: &[],
        });
        assert!(!none.contains("Authorization") && none.contains("Content-Type"));
    }
}
