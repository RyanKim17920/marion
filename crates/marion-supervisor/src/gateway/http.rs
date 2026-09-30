//! The little HTTP/1.1 the gateway needs on its harness side: read one request, write one response
//! — whole, or as a chunked stream. One request per connection (`Connection: close`), so there is no
//! keep-alive state to get wrong; a harness's HTTP client opens the next connection itself.

use std::io::{self, BufRead, Read, Write};

/// A request's head may not exceed this; nothing a harness sends comes near it.
const MAX_HEAD: usize = 64 * 1024;
/// A request's body may not exceed this: a long conversation with images, with room to spare.
pub const MAX_BODY: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// The target without its query (`/v1/messages?beta=true` is `/v1/messages`).
    pub path: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// A header's value, by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// Read one line of the head, charging it to `budget`.
fn head_line(r: &mut impl BufRead, budget: &mut usize) -> io::Result<String> {
    let mut line = Vec::new();
    let n = r
        .by_ref()
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if n > *budget {
        return Err(bad("request head too large"));
    }
    *budget -= n;
    if n == 0 {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    Ok(String::from_utf8_lossy(&line).trim_end().to_string())
}

/// Read one request: its head, then a `Content-Length` or chunked body, each bounded. The gateway
/// reads the two apart, the credential between ([`read_head`]); this is the whole of it, for tests.
#[cfg(test)]
pub fn read_request(r: &mut impl BufRead) -> io::Result<Request> {
    let mut req = read_head(r)?;
    read_body(r, &mut req)?;
    Ok(req)
}

/// Read a request's head alone, its body left unread: what the gateway checks the credential
/// against **before** it reads, or allocates for, a body an unauthenticated caller sent.
pub fn read_head(r: &mut impl BufRead) -> io::Result<Request> {
    let mut budget = MAX_HEAD;
    let line = head_line(r, &mut budget)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(bad("malformed request line"));
    };
    let path = target.split('?').next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let h = head_line(r, &mut budget)?;
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Request {
        method: method.to_string(),
        path,
        headers,
        body: Vec::new(),
    })
}

/// Read the body [`read_head`] left: `Content-Length` or chunked, bounded by [`MAX_BODY`].
pub fn read_body(r: &mut impl BufRead, req: &mut Request) -> io::Result<()> {
    if req
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        req.body = read_chunked(r)?;
    } else if let Some(n) = req.header("content-length") {
        let n: usize = n.parse().map_err(|_| bad("bad content-length"))?;
        if n > MAX_BODY {
            return Err(bad("request body too large"));
        }
        let mut body = vec![0; n];
        r.read_exact(&mut body)?;
        req.body = body;
    }
    Ok(())
}

fn read_chunked(r: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut budget = MAX_HEAD;
        let size = head_line(r, &mut budget)?;
        let size = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| bad("bad chunk size"))?;
        if size == 0 {
            // Trailers, up to the blank line.
            while !head_line(r, &mut budget)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > MAX_BODY {
            return Err(bad("request body too large"));
        }
        let at = body.len();
        body.resize(at + size, 0);
        r.read_exact(&mut body[at..])?;
        head_line(r, &mut budget)?;
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        529 => "Overloaded",
        _ => "Status",
    }
}

/// A whole response. `extra` are further header lines (`retry-after`, say).
pub fn respond(
    w: &mut impl Write,
    status: u16,
    content_type: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        reason(status),
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// A streamed response: the head now, each [`Self::send`] one chunk, flushed at once — the harness
/// sees each event as the provider produces it — and [`Self::end`] the terminating chunk.
pub struct Chunked<W: Write> {
    w: W,
}

impl<W: Write> Chunked<W> {
    pub fn start(mut w: W, content_type: &str) -> io::Result<Self> {
        w.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\n\
                 Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )?;
        w.flush()?;
        Ok(Chunked { w })
    }

    pub fn send(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.w
            .write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
        self.w.write_all(data)?;
        self.w.write_all(b"\r\n")?;
        self.w.flush()
    }

    pub fn end(mut self) -> io::Result<()> {
        self.w.write_all(b"0\r\n\r\n")?;
        self.w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_with_its_query_dropped_and_its_body_exact() {
        let raw = b"POST /v1/messages?beta=true HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\n\
                    Content-Length: 7\r\n\r\n{\"a\":1}";
        let req = read_request(&mut &raw[..]).unwrap();
        assert_eq!(
            (req.method.as_str(), req.path.as_str()),
            ("POST", "/v1/messages")
        );
        assert_eq!(req.header("authorization"), Some("Bearer t"));
        assert_eq!(req.body, br#"{"a":1}"#);
    }

    #[test]
    fn a_chunked_body_is_reassembled_and_an_oversized_one_refused() {
        let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(read_request(&mut &raw[..]).unwrap().body, b"abcde");
        let raw = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(read_request(&mut raw.as_bytes()).is_err());
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(read_request(&mut huge.as_bytes()).is_err());
    }

    #[test]
    fn a_chunked_response_frames_each_send_and_ends_with_the_zero_chunk() {
        let mut out = Vec::new();
        let mut c = Chunked::start(&mut out, "text/event-stream").unwrap();
        c.send(b"hello").unwrap();
        c.send(b"").unwrap();
        c.end().unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "{s}");
        assert!(s.ends_with("\r\n\r\n5\r\nhello\r\n0\r\n\r\n"), "{s}");
    }
}
