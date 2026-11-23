//! The smallest HTTP/1.1 this dashboard needs, and no more.
//!
//! One request per connection, `Connection: close`, bounded sizes and a
//! deadline. Hand-rolled because the alternative is a server framework in a
//! binary whose point is to be small, for a page that one person on this
//! computer opens.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Request line plus headers.
pub const MAX_HEAD: usize = 16 * 1024;
/// The dashboard only ever POSTs a small JSON object.
pub const MAX_BODY: usize = 8 * 1024;
/// A client that has not sent its request by then is dropped.
pub const READ_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Headers beyond the fixed set [`encode`] always writes (the router's
    /// response signature). Names are compile-time constants; values must
    /// not contain line breaks, which [`encode`] enforces.
    pub extra: Vec<(&'static str, String)>,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
            extra: Vec::new(),
        }
    }

    /// The same response with one more header.
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.extra.push((name, value.into()));
        self
    }

    pub fn text(status: u16, body: &str) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            body.as_bytes().to_vec(),
        )
    }

    pub fn json(status: u16, v: &serde_json::Value) -> Self {
        Self::new(
            status,
            "application/json",
            serde_json::to_vec(v).unwrap_or_default(),
        )
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        _ => "Error",
    }
}

/// The content security policy: this page, nothing else. No inline script
/// or style, no other origin, no framing, no form posts.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                       img-src 'self' data:; font-src 'self'; connect-src 'self'; \
                       base-uri 'none'; \
                       form-action 'none'; frame-ancestors 'none'";

/// Serialize a response with the headers every answer carries.
pub fn encode(r: &Response) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Content-Security-Policy: {CSP}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Referrer-Policy: no-referrer\r\n\
         Cache-Control: no-store\r\n\
         Cross-Origin-Opener-Policy: same-origin\r\n\
         Cross-Origin-Resource-Policy: same-origin\r\n\
         Permissions-Policy: camera=(), microphone=(), geolocation=()\r\n\
         Connection: close\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len()
    );
    let mut head = head;
    for (name, value) in &r.extra {
        // A header value with a line break would let it write headers of
        // its own; such a header is dropped rather than sent.
        if value.contains(['\r', '\n']) || name.contains(['\r', '\n', ':']) {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&r.body);
    out
}

fn unhex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Percent-decoding for query values (`+` is a space).
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (unhex(b[i + 1]), unhex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a request head (everything before the blank line).
pub fn parse_head(head: &str) -> Result<Request, &'static str> {
    let mut lines = head.split("\r\n");
    let first = lines.next().ok_or("empty request")?;
    let mut parts = first.split(' ');
    let method = parts.next().ok_or("no method")?.to_string();
    let target = parts.next().ok_or("no target")?;
    let version = parts.next().ok_or("no version")?;
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return Err("bad request line");
    }
    if !target.starts_with('/') {
        // Absolute-form and authority-form targets are for proxies.
        return Err("bad target");
    }
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let query = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once(':').ok_or("bad header")?;
        if k.is_empty() || k.contains(' ') {
            return Err("bad header");
        }
        headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
    }
    Ok(Request {
        method,
        path: path.to_string(),
        query,
        headers,
        body: Vec::new(),
    })
}

/// Read one request from `stream`, within the size limits. The caller wraps
/// this in [`READ_DEADLINE`].
pub async fn read_request<S: AsyncReadExt + Unpin>(stream: &mut S) -> Result<Request, u16> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(413);
        }
        let n = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| 400u16)?;
    let mut req = parse_head(head).map_err(|_| 400u16)?;
    let len: usize = match req.header("content-length") {
        Some(v) => v.parse().map_err(|_| 400u16)?,
        None => 0,
    };
    if req.header("transfer-encoding").is_some() {
        return Err(400);
    }
    if len > MAX_BODY {
        return Err(413);
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    req.body = body;
    Ok(req)
}

pub async fn write_response<S: AsyncWriteExt + Unpin>(stream: &mut S, r: &Response) {
    let _ = stream.write_all(&encode(r)).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_head_parses_into_parts() {
        let r = parse_head(
            "GET /api/state?t=ab%20c&x HTTP/1.1\r\nHost: 127.0.0.1:5\r\nX-Kmplify-Node-Token: k",
        )
        .unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/api/state");
        assert_eq!(r.query("t"), Some("ab c"));
        assert_eq!(r.query("x"), Some(""));
        assert_eq!(r.header("host"), Some("127.0.0.1:5"));
        assert_eq!(r.header("x-kmplify-node-token"), Some("k"));
    }

    #[test]
    fn proxy_targets_and_junk_are_refused() {
        assert!(parse_head("GET http://evil/ HTTP/1.1").is_err());
        assert!(parse_head("GET / SPDY/3").is_err());
        assert!(parse_head("GET / HTTP/1.1\r\nno colon").is_err());
        assert!(parse_head("").is_err());
    }

    #[test]
    fn percent_decoding_is_total() {
        assert_eq!(decode("a%2Fb"), "a/b");
        assert_eq!(decode("%"), "%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%4"), "%4");
    }

    #[test]
    fn every_answer_carries_the_lockdown_headers() {
        let text = String::from_utf8(encode(&Response::text(200, "hi"))).unwrap();
        for h in [
            "Content-Security-Policy: default-src 'none'",
            "X-Content-Type-Options: nosniff",
            "X-Frame-Options: DENY",
            "Referrer-Policy: no-referrer",
            "Cache-Control: no-store",
            "Connection: close",
        ] {
            assert!(text.contains(h), "{h}");
        }
        assert!(!text.to_ascii_lowercase().contains("access-control-allow"));
    }
}
